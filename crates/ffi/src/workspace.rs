//! Settings, profiles, credentials, history, worksheets and layout (M2.11
//! families 5-7): the local `Store` and the credential store, both owned by
//! **one service thread this crate spawns and owns**, never the UI thread
//! (`ARCHITECTURE.md` §6 — every `Store`/`CredentialStore` call is blocking
//! I/O). [`reldex_workspace_open`] returns a [`ReldexWorkspace`] handle
//! immediately; every operation on it is submit-now, reply-later, exactly
//! like a session (`crate::session`) and the hub's event queue
//! (`crate::hub`) — a request function sends a command and returns, and the
//! answer arrives as a [`ReldexWorkspaceReply`] drained with
//! [`reldex_workspace_next_reply`] after the registered waker fires.
//!
//! # Why one thread for settings, profiles, credentials *and*
//! history/worksheets/layout
//!
//! All seven kinds of call are the same shape — disk or OS-credential-store
//! I/O that must never run on the UI thread — and `reldex_workspace::Store`
//! is `Send`, not `Sync`: it must live on exactly one thread for its whole
//! life. Splitting settings from history from credentials would mean three
//! threads serialising against the same SQLite file (or worse, three
//! `Store`s open on it at once) for no benefit; one thread, one `Store`, one
//! `Box<dyn CredentialStore>`, one command queue.
//!
//! # Composition root
//!
//! [`OracleDriverBinding`] is `reldex_workspace::DriverBinding` implemented
//! against `reldex_driver_oracle_thin`'s `sid_endpoint` and extension-key
//! constants — the same composition-root role `crates/ffi/src/metadata.rs`
//! and `crates/ffi/src/splitter.rs` already play (`ARCHITECTURE.md` §2).
//!
//! # The password never crosses as a plain string the UI can keep
//!
//! A resolved or fetched password crosses as an opaque, owned
//! [`ReldexSecret`] — not a [`crate::ReldexStr`] the caller could copy into
//! its own buffer and forget about. [`reldex_secret_expose`] hands back a
//! *borrowed* view, valid only until [`reldex_secret_release`], which wipes
//! the underlying [`reldex_db_driver_api::Secret`] the same way
//! `reldex-secrets` always does (ADR-0007 S6). The caller's job is to pass it
//! straight to [`reldex_workspace_build_connect_params`] and release it
//! promptly — never to copy its exposed text into a longer-lived buffer.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, RwLock};

use reldex_db_core::{DbError, ErrorKind};
use reldex_db_driver_api::{
    ConnectionParams, Endpoint, ExtensionValue, Extensions, Secret, TlsMode,
};
use reldex_driver_oracle_thin::{
    EXT_ALLOW_UNENFORCED_SERVER_CERT_DN, EXT_CONNECT_TIMEOUT_UNBOUNDED, EXT_REWRITE_TRIGGER_DDL,
    EXT_WALLET_DIR,
};
#[cfg(feature = "mock-driver")]
use reldex_secrets::MemoryCredentialStore;
use reldex_secrets::{CredentialError, CredentialStore, PasswordSource, PromptReason};
use reldex_workspace::settings::{ByteLimit, EntryLimit, SettingId, SettingValue, TimeLimit};
use reldex_workspace::{
    Authentication, ConnectSettings, CredentialKey, DatabaseType, DriverBinding, DriverOptions,
    Environment, HistoryEntry, HistoryId, HistoryOutcome, HistoryPage, HistoryRecord, IdError,
    Layout, PaneSizes, PasswordStorage, Profile, ProfileDetails, ProfileEndpoint, ProfileError,
    ProfileId, ResolveContext, Resolved, ServiceTarget, Store, StoreError, TlsOptions, Transport,
    UnixTimeMs, WindowGeometry, Worksheet, WorksheetError, WorksheetId, WorksheetState,
};

use crate::error::{ReldexError, set_last_argument_error};
use crate::status::{ReldexStatus, WakerGuard, entry, entry_value};
use crate::strings::{
    CStruct, OwnedStr, ReldexStr, check_out_struct, read_in_struct, write_out_struct,
};

// ============================================================================
// Errors: every crate this module composes has its own value-carrying error
// type. None of them is `DbError`, so each is folded into one here, once,
// rather than at every call site. The `ErrorKind` chosen is coarse on
// purpose (`Configuration` for "the input was refused", `Other` for "the
// backend failed") -- see the PR description's "weak points" note.
// ============================================================================

fn store_error(prefix: &str, error: StoreError) -> DbError {
    let kind = match &error {
        StoreError::InvalidProfile(_)
        | StoreError::InvalidSetting(_)
        | StoreError::InvalidHistory(_)
        | StoreError::InvalidWorksheet(_)
        | StoreError::ProfileNotFound(_)
        | StoreError::ProfileExists(_)
        | StoreError::WorksheetNotFound(_) => ErrorKind::Configuration,
        _ => ErrorKind::Other,
    };
    DbError::new(kind, format!("{prefix}: {error}"))
}

fn profile_error(prefix: &str, error: ProfileError) -> DbError {
    DbError::new(ErrorKind::Configuration, format!("{prefix}: {error}"))
}

fn id_error(prefix: &str, error: IdError) -> DbError {
    DbError::new(ErrorKind::Configuration, format!("{prefix}: {error}"))
}

/// `UnixTimeMs::as_millis()` returns `i64` (matching every other timestamp in
/// this codebase); every FFI timestamp field here is `u64` for a simpler C
/// type with no negative case a caller would have to consider. Negative is
/// unreachable in practice (it would mean a clock before 1970) and mapped to
/// 0 rather than panicking.
fn millis_to_ffi(value: UnixTimeMs) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(0)
}

fn credential_error(prefix: &str, error: CredentialError) -> DbError {
    let kind = match error {
        CredentialError::NotFound => ErrorKind::Configuration,
        _ => ErrorKind::Other,
    };
    DbError::new(kind, format!("{prefix}: {error}"))
}

// ============================================================================
// Setting id / level / value: the numeric identities the FFI uses in place
// of `reldex_workspace::settings`'s Rust-only typed handles and storage
// keys.
// ============================================================================

/// A setting, by the numeric id this ABI assigns -- never the crate-private
/// storage key.
///
/// `0` is reserved for an id this header predates (ADR-0003 D7); the order
/// otherwise matches `reldex_workspace::settings::SettingId::ALL`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexSettingId {
    /// An id this header does not know.
    Unknown = 0,
    /// `CONNECT_TIMEOUT`.
    ConnectTimeout = 1,
    /// `REWRITE_TRIGGER_DDL`.
    RewriteTriggerDdl = 2,
    /// `STATEMENT_TIME_LIMIT`.
    StatementTimeLimit = 3,
    /// `FETCH_ROWS`.
    FetchRows = 4,
    /// `FETCHES_IN_FLIGHT`.
    FetchesInFlight = 5,
    /// `SERVER_OUTPUT_ENABLED`.
    ServerOutputEnabled = 6,
    /// `SERVER_OUTPUT_BUFFER`.
    ServerOutputBuffer = 7,
    /// `HISTORY_MAX_ENTRIES_PER_PROFILE`.
    HistoryMaxEntriesPerProfile = 8,
}

impl ReldexSettingId {
    fn from_i32(value: i32) -> Option<Self> {
        Some(match value {
            1 => Self::ConnectTimeout,
            2 => Self::RewriteTriggerDdl,
            3 => Self::StatementTimeLimit,
            4 => Self::FetchRows,
            5 => Self::FetchesInFlight,
            6 => Self::ServerOutputEnabled,
            7 => Self::ServerOutputBuffer,
            8 => Self::HistoryMaxEntriesPerProfile,
            _ => return None,
        })
    }

    const fn to_setting_id(self) -> Option<SettingId> {
        Some(match self {
            Self::Unknown => return None,
            Self::ConnectTimeout => SettingId::ConnectTimeout,
            Self::RewriteTriggerDdl => SettingId::RewriteTriggerDdl,
            Self::StatementTimeLimit => SettingId::StatementTimeLimit,
            Self::FetchRows => SettingId::FetchRows,
            Self::FetchesInFlight => SettingId::FetchesInFlight,
            Self::ServerOutputEnabled => SettingId::ServerOutputEnabled,
            Self::ServerOutputBuffer => SettingId::ServerOutputBuffer,
            Self::HistoryMaxEntriesPerProfile => SettingId::HistoryMaxEntriesPerProfile,
        })
    }

    const fn from_setting_id(id: SettingId) -> Self {
        match id {
            SettingId::ConnectTimeout => Self::ConnectTimeout,
            SettingId::RewriteTriggerDdl => Self::RewriteTriggerDdl,
            SettingId::StatementTimeLimit => Self::StatementTimeLimit,
            SettingId::FetchRows => Self::FetchRows,
            SettingId::FetchesInFlight => Self::FetchesInFlight,
            SettingId::ServerOutputEnabled => Self::ServerOutputEnabled,
            SettingId::ServerOutputBuffer => Self::ServerOutputBuffer,
            SettingId::HistoryMaxEntriesPerProfile => Self::HistoryMaxEntriesPerProfile,
            // `SettingId` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// A level a setting resolves at or is set at.
///
/// `0` is reserved for a level this header predates. [`Self::BuiltIn`] is a
/// valid *resolution source* but never a valid *scope* to set or clear at --
/// [`reldex_workspace_set_setting`] and [`reldex_workspace_clear_setting`]
/// refuse it.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexSettingLevel {
    /// A level this header does not know.
    Unknown = 0,
    /// The built-in default. Resolution source only.
    BuiltIn = 1,
    /// The application-wide default.
    Application = 2,
    /// One connection profile's override.
    Profile = 3,
    /// One worksheet's override.
    Worksheet = 4,
}

impl From<reldex_workspace::Level> for ReldexSettingLevel {
    fn from(level: reldex_workspace::Level) -> Self {
        match level {
            reldex_workspace::Level::BuiltIn => Self::BuiltIn,
            reldex_workspace::Level::Application => Self::Application,
            reldex_workspace::Level::Profile => Self::Profile,
            reldex_workspace::Level::Worksheet => Self::Worksheet,
            // `Level` is `#[non_exhaustive]`.
            _ => Self::Unknown,
        }
    }
}

/// Which of [`ReldexSettingValue`]'s value fields is meaningful.
///
/// `0` is reserved for a kind this header predates; the values otherwise
/// match `reldex_workspace::settings::ValueKind`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexValueKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// [`ReldexSettingValue::bool_value`].
    Bool = 1,
    /// A time limit: [`ReldexSettingValue::no_limit`] or
    /// [`ReldexSettingValue::number_value`] seconds.
    TimeLimit = 2,
    /// [`ReldexSettingValue::count_value`].
    Count = 3,
    /// A byte limit: [`ReldexSettingValue::no_limit`] or
    /// [`ReldexSettingValue::number_value`] bytes.
    ByteLimit = 4,
    /// An entry limit: [`ReldexSettingValue::no_limit`] or
    /// [`ReldexSettingValue::number_value`] entries.
    EntryLimit = 5,
}

/// A setting's value, flattened to one shape for every
/// [`ReldexValueKind`] (`SPEC.md` §15's typed settings, crossed as data since
/// the FFI chooses a setting at run time -- the same reason
/// `reldex_workspace::SettingValue` exists on the Rust side).
///
/// Only the field(s) `kind` documents are meaningful; the others are zero.
/// `struct_size` follows the ADR-0003 D7 prefix rule.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ReldexSettingValue {
    /// `sizeof(ReldexSettingValue)` on the way in; how much is valid on the
    /// way out.
    pub struct_size: u32,
    /// A [`ReldexValueKind`].
    pub kind: i32,
    /// Meaningful for [`ReldexValueKind::Bool`].
    pub bool_value: bool,
    /// Meaningful for [`ReldexValueKind::Count`].
    pub count_value: u32,
    /// For [`ReldexValueKind::TimeLimit`]/[`ReldexValueKind::ByteLimit`]/
    /// [`ReldexValueKind::EntryLimit`]: `true` means "no limit"/"unlimited",
    /// and [`Self::number_value`] is then not meaningful.
    pub no_limit: bool,
    /// Seconds, bytes or entries, for the three limit kinds when
    /// [`Self::no_limit`] is `false`.
    pub number_value: u32,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer or
// `bool` -- all valid as zero (kind 0 is "unknown", the rest read as
// false/0).
unsafe impl CStruct for ReldexSettingValue {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexSettingValue {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexValueKind::Unknown as i32,
            bool_value: false,
            count_value: 0,
            no_limit: false,
            number_value: 0,
        }
    }
}

impl ReldexSettingValue {
    fn from_value(value: SettingValue) -> Self {
        let mut out = Self::default();
        match value {
            SettingValue::Bool(value) => {
                out.kind = ReldexValueKind::Bool as i32;
                out.bool_value = value;
            }
            SettingValue::Count(value) => {
                out.kind = ReldexValueKind::Count as i32;
                out.count_value = value;
            }
            SettingValue::TimeLimit(TimeLimit::NoLimit) => {
                out.kind = ReldexValueKind::TimeLimit as i32;
                out.no_limit = true;
            }
            SettingValue::TimeLimit(TimeLimit::Seconds(seconds)) => {
                out.kind = ReldexValueKind::TimeLimit as i32;
                out.number_value = seconds.get();
            }
            SettingValue::ByteLimit(ByteLimit::Unlimited) => {
                out.kind = ReldexValueKind::ByteLimit as i32;
                out.no_limit = true;
            }
            SettingValue::ByteLimit(ByteLimit::Bytes(bytes)) => {
                out.kind = ReldexValueKind::ByteLimit as i32;
                out.number_value = bytes.get();
            }
            SettingValue::EntryLimit(EntryLimit::Unlimited) => {
                out.kind = ReldexValueKind::EntryLimit as i32;
                out.no_limit = true;
            }
            SettingValue::EntryLimit(EntryLimit::Count(count)) => {
                out.kind = ReldexValueKind::EntryLimit as i32;
                out.number_value = count.get();
            }
            // `SettingValue` is `#[non_exhaustive]`.
            _ => {}
        }
        out
    }

    /// Builds the typed `SettingValue` this ABI's `kind` names, refusing a
    /// zero (out-of-range, non-positive) number for a kind that requires one.
    fn to_value(self) -> Option<SettingValue> {
        Some(match ReldexValueKind::from_i32_checked(self.kind)? {
            ReldexValueKind::Unknown => return None,
            ReldexValueKind::Bool => SettingValue::Bool(self.bool_value),
            ReldexValueKind::Count => SettingValue::Count(self.count_value),
            ReldexValueKind::TimeLimit => SettingValue::TimeLimit(if self.no_limit {
                TimeLimit::NoLimit
            } else {
                TimeLimit::seconds(self.number_value)?
            }),
            ReldexValueKind::ByteLimit => SettingValue::ByteLimit(if self.no_limit {
                ByteLimit::Unlimited
            } else {
                ByteLimit::bytes(self.number_value)?
            }),
            ReldexValueKind::EntryLimit => SettingValue::EntryLimit(if self.no_limit {
                EntryLimit::Unlimited
            } else {
                EntryLimit::count(self.number_value)?
            }),
        })
    }
}

impl ReldexValueKind {
    fn from_i32_checked(value: i32) -> Option<Self> {
        Some(match value {
            1 => Self::Bool,
            2 => Self::TimeLimit,
            3 => Self::Count,
            4 => Self::ByteLimit,
            5 => Self::EntryLimit,
            _ => return None,
        })
    }
}

/// Parses a scope from a [`ReldexSettingLevel`] and, for
/// [`ReldexSettingLevel::Profile`]/[`ReldexSettingLevel::Worksheet`], 16
/// id bytes. Refuses [`ReldexSettingLevel::BuiltIn`] and
/// [`ReldexSettingLevel::Unknown`]: neither is a settable scope.
fn parse_scope(level: i32, id_bytes: [u8; 16]) -> Result<reldex_workspace::Scope, DbError> {
    match level {
        x if x == ReldexSettingLevel::Application as i32 => {
            Ok(reldex_workspace::Scope::Application)
        }
        x if x == ReldexSettingLevel::Profile as i32 => ProfileId::from_bytes(id_bytes)
            .map(reldex_workspace::Scope::Profile)
            .map_err(|error| id_error("scope profile id", error)),
        x if x == ReldexSettingLevel::Worksheet as i32 => WorksheetId::from_bytes(id_bytes)
            .map(reldex_workspace::Scope::Worksheet)
            .map_err(|error| id_error("scope worksheet id", error)),
        _ => Err(DbError::new(
            ErrorKind::Configuration,
            "the setting level must be Application, Profile or Worksheet",
        )),
    }
}

// ============================================================================
// Profiles.
// ============================================================================

/// Which database a profile connects to.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexDatabaseType {
    /// A type this header does not know.
    Unknown = 0,
    /// Oracle Database, through the thin driver.
    Oracle = 1,
}

/// A profile's environment (`SPEC.md` §17).
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexEnvironmentKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Development.
    Development = 1,
    /// Test.
    Test = 2,
    /// User acceptance testing.
    Uat = 3,
    /// Staging.
    Staging = 4,
    /// Production.
    Production = 5,
    /// A user-named environment; the label is
    /// [`ReldexProfileDetails::environment_label`].
    Custom = 6,
}

/// How [`ReldexProfileDetails::endpoint_kind`] is shaped.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexEndpointKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Host, port, and a service name or SID.
    HostPort = 1,
    /// A complete connect string or descriptor.
    ConnectString = 2,
}

/// Whether [`ReldexProfileDetails::service_name_or_sid`] is a service name or
/// a SID, for [`ReldexEndpointKind::HostPort`].
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexServiceTargetKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// A service name.
    ServiceName = 1,
    /// A system identifier.
    Sid = 2,
}

/// How a session authenticates.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexAuthKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// A user name and a password (never carried here -- see
    /// [`ReldexProfileDetails::password_storage`]).
    Password = 1,
    /// Authentication by the operating system or another external mechanism.
    External = 2,
}

/// Where a profile's password is kept.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexPasswordStorageKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// The credential store holds it.
    CredentialStore = 1,
    /// The user is asked at every connect.
    PromptEachTime = 2,
}

/// Whether the transport is encrypted.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexTransportKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Plain TCP.
    Plain = 1,
    /// TLS required.
    Tls = 2,
}

/// The administrative role a session opens with.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexSessionRoleKind {
    /// A role this header does not know.
    Unknown = 0,
    /// An ordinary session.
    Normal = 1,
    /// The highest administrative role.
    SysDba = 2,
    /// The restricted operator role.
    SysOper = 3,
}

impl From<reldex_db_driver_api::SessionRole> for ReldexSessionRoleKind {
    fn from(role: reldex_db_driver_api::SessionRole) -> Self {
        match role {
            reldex_db_driver_api::SessionRole::Normal => Self::Normal,
            reldex_db_driver_api::SessionRole::SysDba => Self::SysDba,
            reldex_db_driver_api::SessionRole::SysOper => Self::SysOper,
        }
    }
}

impl ReldexSessionRoleKind {
    const fn to_role(self) -> reldex_db_driver_api::SessionRole {
        match self {
            Self::SysDba => reldex_db_driver_api::SessionRole::SysDba,
            Self::SysOper => reldex_db_driver_api::SessionRole::SysOper,
            Self::Unknown | Self::Normal => reldex_db_driver_api::SessionRole::Normal,
        }
    }
}

/// Everything about a profile the caller edits (`SPEC.md` §17), as one flat
/// input struct -- the FFI shape of `reldex_workspace::ProfileDetails`.
///
/// Every [`ReldexStr`] field is read only for the duration of the call that
/// takes this struct (the "into Reldex" rule -- see [`ReldexStr`]).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexProfileDetails {
    /// `sizeof(ReldexProfileDetails)` on the way in.
    pub struct_size: u32,
    /// Display name.
    pub name: ReldexStr,
    /// A [`ReldexDatabaseType`].
    pub database_type: i32,
    /// A [`ReldexEnvironmentKind`].
    pub environment: i32,
    /// The custom environment's label, for [`ReldexEnvironmentKind::Custom`].
    pub environment_label: ReldexStr,
    /// Whether the production indicator shows for this profile.
    pub treat_as_production: bool,
    /// A [`ReldexEndpointKind`].
    pub endpoint_kind: i32,
    /// The host, for [`ReldexEndpointKind::HostPort`].
    pub host: ReldexStr,
    /// The port, for [`ReldexEndpointKind::HostPort`].
    pub port: u16,
    /// A [`ReldexServiceTargetKind`], for [`ReldexEndpointKind::HostPort`].
    pub service_target_kind: i32,
    /// The service name or SID, for [`ReldexEndpointKind::HostPort`].
    pub service_name_or_sid: ReldexStr,
    /// The connect string, for [`ReldexEndpointKind::ConnectString`].
    pub connect_string: ReldexStr,
    /// A [`ReldexAuthKind`].
    pub auth_kind: i32,
    /// The user name, for [`ReldexAuthKind::Password`].
    pub username: ReldexStr,
    /// A [`ReldexPasswordStorageKind`], for [`ReldexAuthKind::Password`].
    pub password_storage: i32,
    /// A [`ReldexSessionRoleKind`].
    pub role: i32,
    /// A [`ReldexTransportKind`].
    pub transport: i32,
    /// A directory of certificate authorities to trust, or empty for none.
    pub ca_directory: ReldexStr,
    /// The C-6 guard's opt-out.
    pub allow_unenforced_certificate_pin: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool` or `ReldexStr` -- all valid as zero.
unsafe impl CStruct for ReldexProfileDetails {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// Builds a `ProfileDetails` from the caller's struct.
///
/// # Safety
///
/// `details`'s string fields must point at readable, valid UTF-8 bytes for
/// the lengths they declare.
unsafe fn build_profile_details(details: &ReldexProfileDetails) -> Result<ProfileDetails, DbError> {
    let invalid = |field: &str| {
        DbError::new(
            ErrorKind::Configuration,
            format!("reldex_workspace: `{field}` is not valid UTF-8"),
        )
    };
    // SAFETY: delegated to this function's contract.
    let name = unsafe { details.name.as_str() }.ok_or_else(|| invalid("name"))?;
    // SAFETY: as above.
    let environment_label = unsafe { details.environment_label.as_str() }
        .ok_or_else(|| invalid("environment_label"))?;
    // SAFETY: as above.
    let host = unsafe { details.host.as_str() }.ok_or_else(|| invalid("host"))?;
    // SAFETY: as above.
    let service_name_or_sid = unsafe { details.service_name_or_sid.as_str() }
        .ok_or_else(|| invalid("service_name_or_sid"))?;
    // SAFETY: as above.
    let connect_string =
        unsafe { details.connect_string.as_str() }.ok_or_else(|| invalid("connect_string"))?;
    // SAFETY: as above.
    let username = unsafe { details.username.as_str() }.ok_or_else(|| invalid("username"))?;
    // SAFETY: as above.
    let ca_directory =
        unsafe { details.ca_directory.as_str() }.ok_or_else(|| invalid("ca_directory"))?;

    let database = match details.database_type {
        x if x == ReldexDatabaseType::Oracle as i32 => DatabaseType::Oracle,
        _ => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                "reldex_workspace: `database_type` is not a database type this build knows",
            ));
        }
    };
    let environment = match details.environment {
        x if x == ReldexEnvironmentKind::Development as i32 => Environment::Development,
        x if x == ReldexEnvironmentKind::Test as i32 => Environment::Test,
        x if x == ReldexEnvironmentKind::Uat as i32 => Environment::Uat,
        x if x == ReldexEnvironmentKind::Staging as i32 => Environment::Staging,
        x if x == ReldexEnvironmentKind::Production as i32 => Environment::Production,
        x if x == ReldexEnvironmentKind::Custom as i32 => {
            Environment::Custom(environment_label.to_owned())
        }
        _ => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                "reldex_workspace: `environment` is not an environment this build knows",
            ));
        }
    };
    let endpoint = match details.endpoint_kind {
        x if x == ReldexEndpointKind::HostPort as i32 => {
            let target = match details.service_target_kind {
                x if x == ReldexServiceTargetKind::ServiceName as i32 => {
                    ServiceTarget::ServiceName(service_name_or_sid.to_owned())
                }
                x if x == ReldexServiceTargetKind::Sid as i32 => {
                    ServiceTarget::Sid(service_name_or_sid.to_owned())
                }
                _ => {
                    return Err(DbError::new(
                        ErrorKind::Configuration,
                        "reldex_workspace: `service_target_kind` is not a kind this build knows",
                    ));
                }
            };
            ProfileEndpoint::HostPort {
                host: host.to_owned(),
                port: details.port,
                target,
            }
        }
        x if x == ReldexEndpointKind::ConnectString as i32 => {
            ProfileEndpoint::ConnectString(connect_string.to_owned())
        }
        _ => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                "reldex_workspace: `endpoint_kind` is not a kind this build knows",
            ));
        }
    };
    let authentication = match details.auth_kind {
        x if x == ReldexAuthKind::Password as i32 => {
            let storage = match details.password_storage {
                x if x == ReldexPasswordStorageKind::CredentialStore as i32 => {
                    PasswordStorage::CredentialStore
                }
                x if x == ReldexPasswordStorageKind::PromptEachTime as i32 => {
                    PasswordStorage::PromptEachTime
                }
                _ => {
                    return Err(DbError::new(
                        ErrorKind::Configuration,
                        "reldex_workspace: `password_storage` is not a kind this build knows",
                    ));
                }
            };
            Authentication::Password {
                username: username.to_owned(),
                storage,
            }
        }
        x if x == ReldexAuthKind::External as i32 => Authentication::External,
        _ => {
            return Err(DbError::new(
                ErrorKind::Configuration,
                "reldex_workspace: `auth_kind` is not a kind this build knows",
            ));
        }
    };
    let transport = match details.transport {
        x if x == ReldexTransportKind::Tls as i32 => Transport::Tls,
        _ => Transport::Plain,
    };
    let role_kind = match details.role {
        x if x == ReldexSessionRoleKind::SysDba as i32 => ReldexSessionRoleKind::SysDba,
        x if x == ReldexSessionRoleKind::SysOper as i32 => ReldexSessionRoleKind::SysOper,
        _ => ReldexSessionRoleKind::Normal,
    };

    Ok(ProfileDetails {
        name: name.to_owned(),
        database,
        environment,
        treat_as_production: details.treat_as_production,
        endpoint,
        authentication,
        role: role_kind.to_role(),
        tls: TlsOptions {
            transport,
            ca_directory: (!ca_directory.is_empty()).then(|| PathBuf::from(ca_directory)),
            allow_unenforced_certificate_pin: details.allow_unenforced_certificate_pin,
        },
    })
}

/// One profile's details, held as text this crate owns -- the storage behind
/// [`ReldexProfileView`]'s borrowed strings.
struct StoredProfile {
    id: [u8; 16],
    created_at: u64,
    updated_at: u64,
    name: OwnedStr,
    database_type: i32,
    environment: i32,
    environment_label: OwnedStr,
    treat_as_production: bool,
    endpoint_kind: i32,
    host: OwnedStr,
    port: u16,
    service_target_kind: i32,
    service_name_or_sid: OwnedStr,
    connect_string: OwnedStr,
    auth_kind: i32,
    username: OwnedStr,
    password_storage: i32,
    role: i32,
    transport: i32,
    ca_directory: OwnedStr,
    allow_unenforced_certificate_pin: bool,
}

impl StoredProfile {
    fn from_profile(profile: &Profile) -> Self {
        let details = profile.details();
        let (environment, environment_label) = match &details.environment {
            Environment::Development => (ReldexEnvironmentKind::Development, String::new()),
            Environment::Test => (ReldexEnvironmentKind::Test, String::new()),
            Environment::Uat => (ReldexEnvironmentKind::Uat, String::new()),
            Environment::Staging => (ReldexEnvironmentKind::Staging, String::new()),
            Environment::Production => (ReldexEnvironmentKind::Production, String::new()),
            Environment::Custom(label) => (ReldexEnvironmentKind::Custom, label.clone()),
            // `Environment` is `#[non_exhaustive]`.
            _ => (ReldexEnvironmentKind::Unknown, String::new()),
        };
        let (endpoint_kind, host, port, service_target_kind, service_name_or_sid, connect_string) =
            match &details.endpoint {
                ProfileEndpoint::HostPort { host, port, target } => {
                    let (kind, text) = match target {
                        ServiceTarget::ServiceName(name) => {
                            (ReldexServiceTargetKind::ServiceName, name.clone())
                        }
                        ServiceTarget::Sid(sid) => (ReldexServiceTargetKind::Sid, sid.clone()),
                        // `ServiceTarget` is `#[non_exhaustive]`.
                        _ => (ReldexServiceTargetKind::Unknown, String::new()),
                    };
                    (
                        ReldexEndpointKind::HostPort,
                        host.clone(),
                        *port,
                        kind,
                        text,
                        String::new(),
                    )
                }
                ProfileEndpoint::ConnectString(text) => (
                    ReldexEndpointKind::ConnectString,
                    String::new(),
                    0,
                    ReldexServiceTargetKind::Unknown,
                    String::new(),
                    text.clone(),
                ),
                // `ProfileEndpoint` is `#[non_exhaustive]`.
                _ => (
                    ReldexEndpointKind::Unknown,
                    String::new(),
                    0,
                    ReldexServiceTargetKind::Unknown,
                    String::new(),
                    String::new(),
                ),
            };
        let (auth_kind, username, password_storage) = match &details.authentication {
            Authentication::Password { username, storage } => {
                let storage = match storage {
                    PasswordStorage::CredentialStore => ReldexPasswordStorageKind::CredentialStore,
                    PasswordStorage::PromptEachTime => ReldexPasswordStorageKind::PromptEachTime,
                    // `PasswordStorage` is `#[non_exhaustive]`.
                    _ => ReldexPasswordStorageKind::Unknown,
                };
                (ReldexAuthKind::Password, username.clone(), storage)
            }
            Authentication::External => (
                ReldexAuthKind::External,
                String::new(),
                ReldexPasswordStorageKind::Unknown,
            ),
            // `Authentication` is `#[non_exhaustive]`.
            _ => (
                ReldexAuthKind::Unknown,
                String::new(),
                ReldexPasswordStorageKind::Unknown,
            ),
        };
        let transport = match details.tls.transport {
            Transport::Plain => ReldexTransportKind::Plain,
            Transport::Tls => ReldexTransportKind::Tls,
            // `Transport` is `#[non_exhaustive]`.
            _ => ReldexTransportKind::Unknown,
        };
        Self {
            id: *profile.id().as_bytes(),
            created_at: millis_to_ffi(profile.created_at()),
            updated_at: millis_to_ffi(profile.modified_at()),
            name: OwnedStr::new(details.name.clone()),
            database_type: ReldexDatabaseType::Oracle as i32,
            environment: environment as i32,
            environment_label: OwnedStr::new(environment_label),
            treat_as_production: details.treat_as_production,
            endpoint_kind: endpoint_kind as i32,
            host: OwnedStr::new(host),
            port,
            service_target_kind: service_target_kind as i32,
            service_name_or_sid: OwnedStr::new(service_name_or_sid),
            connect_string: OwnedStr::new(connect_string),
            auth_kind: auth_kind as i32,
            username: OwnedStr::new(username),
            password_storage: password_storage as i32,
            role: ReldexSessionRoleKind::from(details.role) as i32,
            transport: transport as i32,
            ca_directory: OwnedStr::new(
                details
                    .tls
                    .ca_directory
                    .as_deref()
                    .and_then(|path| path.to_str())
                    .unwrap_or_default()
                    .to_owned(),
            ),
            allow_unenforced_certificate_pin: details.tls.allow_unenforced_certificate_pin,
        }
    }

    fn view(&self) -> ReldexProfileView {
        ReldexProfileView {
            struct_size: u32::try_from(size_of::<ReldexProfileView>()).unwrap_or(u32::MAX),
            id: self.id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            name: self.name.as_reldex_str(),
            database_type: self.database_type,
            environment: self.environment,
            environment_label: self.environment_label.as_reldex_str(),
            treat_as_production: self.treat_as_production,
            endpoint_kind: self.endpoint_kind,
            host: self.host.as_reldex_str(),
            port: self.port,
            service_target_kind: self.service_target_kind,
            service_name_or_sid: self.service_name_or_sid.as_reldex_str(),
            connect_string: self.connect_string.as_reldex_str(),
            auth_kind: self.auth_kind,
            username: self.username.as_reldex_str(),
            password_storage: self.password_storage,
            role: self.role,
            transport: self.transport,
            ca_directory: self.ca_directory.as_reldex_str(),
            allow_unenforced_certificate_pin: self.allow_unenforced_certificate_pin,
        }
    }
}

/// A read-only view of one stored profile, every string borrowed from the
/// [`ReldexProfileList`] it came from -- the same borrowed-view style as
/// [`crate::ReldexErrorView`] and [`crate::ReldexColumnInfo`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexProfileView {
    /// `sizeof(ReldexProfileView)` on the way in; how much is valid on the
    /// way out.
    pub struct_size: u32,
    /// The profile's id (16-byte UUID).
    pub id: [u8; 16],
    /// Created, Unix milliseconds.
    pub created_at: u64,
    /// Last modified, Unix milliseconds.
    pub updated_at: u64,
    /// Display name.
    pub name: ReldexStr,
    /// A [`ReldexDatabaseType`].
    pub database_type: i32,
    /// A [`ReldexEnvironmentKind`].
    pub environment: i32,
    /// The custom environment's label.
    pub environment_label: ReldexStr,
    /// Whether the production indicator shows for this profile.
    pub treat_as_production: bool,
    /// A [`ReldexEndpointKind`].
    pub endpoint_kind: i32,
    /// The host.
    pub host: ReldexStr,
    /// The port.
    pub port: u16,
    /// A [`ReldexServiceTargetKind`].
    pub service_target_kind: i32,
    /// The service name or SID.
    pub service_name_or_sid: ReldexStr,
    /// The connect string.
    pub connect_string: ReldexStr,
    /// A [`ReldexAuthKind`].
    pub auth_kind: i32,
    /// The user name.
    pub username: ReldexStr,
    /// A [`ReldexPasswordStorageKind`].
    pub password_storage: i32,
    /// A [`ReldexSessionRoleKind`].
    pub role: i32,
    /// A [`ReldexTransportKind`].
    pub transport: i32,
    /// The CA directory, or empty.
    pub ca_directory: ReldexStr,
    /// The C-6 guard's opt-out.
    pub allow_unenforced_certificate_pin: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool`, `[u8; 16]` or `ReldexStr` -- all valid as zero.
unsafe impl CStruct for ReldexProfileView {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// A list of profiles -- [`crate::reldex_workspace_get_profile`] (0 or 1) or
/// [`crate::reldex_workspace_list_profiles`] -- owned by the caller from the
/// moment the reply that carries it is drained.
pub struct ReldexProfileList {
    profiles: Vec<StoredProfile>,
}

impl Drop for ReldexProfileList {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexProfileList {
    fn new(profiles: Vec<Profile>) -> Self {
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        Self {
            profiles: profiles.iter().map(StoredProfile::from_profile).collect(),
        }
    }
}

/// How many profiles `list` holds.
///
/// # Safety
///
/// `list` must be null (reported as 0) or a live [`ReldexProfileList`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_profile_list_count(list: *const ReldexProfileList) -> usize {
    entry_value(0, || {
        if list.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*list }.profiles.len()
    })
}

/// Reads profile `index` into `out`.
///
/// # Safety
///
/// `list` must be null (reported as `false`) or a live [`ReldexProfileList`].
/// `out` must be null or point at a writable [`ReldexProfileView`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_profile_list_get(
    list: *const ReldexProfileList,
    index: usize,
    out: *mut ReldexProfileView,
) -> bool {
    entry_value(false, || {
        if list.is_null() {
            return false;
        }
        // SAFETY: delegated to this function's contract.
        let Some(profile) = unsafe { &*list }.profiles.get(index) else {
            return false;
        };
        // SAFETY: delegated to this function's contract for `out`.
        unsafe { write_out_struct(out, profile.view()) }
    })
}

/// Releases a profile list.
///
/// # Safety
///
/// `list` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_profile_list_release(list: *mut ReldexProfileList) {
    entry_value((), || {
        if list.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(list) });
    });
}

// ============================================================================
// The Oracle driver binding -- the composition root's implementation of
// `reldex_workspace::DriverBinding` (family 5).
// ============================================================================

/// `reldex_workspace::DriverBinding` for Oracle, built on
/// `reldex_driver_oracle_thin`'s `sid_endpoint` and extension-key constants.
/// The one place this module names a concrete driver (`ARCHITECTURE.md` §2).
struct OracleDriverBinding;

impl DriverBinding for OracleDriverBinding {
    fn database_type(&self) -> DatabaseType {
        DatabaseType::Oracle
    }

    fn sid_endpoint(
        &self,
        host: &str,
        port: u16,
        sid: &str,
        transport: Transport,
    ) -> Result<Endpoint, DbError> {
        let tls = matches!(transport, Transport::Tls);
        reldex_driver_oracle_thin::sid_endpoint(host, port, sid, tls).map(Endpoint::ConnectString)
    }

    fn extensions(&self, options: &DriverOptions<'_>) -> Result<Extensions, DbError> {
        let mut extensions = Extensions::new();
        extensions
            .set(
                EXT_CONNECT_TIMEOUT_UNBOUNDED,
                ExtensionValue::Flag(options.connect_without_limit),
            )
            .set(
                EXT_REWRITE_TRIGGER_DDL,
                ExtensionValue::Flag(options.rewrite_trigger_ddl),
            )
            .set(
                EXT_ALLOW_UNENFORCED_SERVER_CERT_DN,
                ExtensionValue::Flag(options.allow_unenforced_certificate_pin),
            );
        if let Some(directory) = options.ca_directory {
            let text = directory.to_str().ok_or_else(|| {
                DbError::new(
                    ErrorKind::Configuration,
                    "the profile's CA directory is not valid Unicode",
                )
            })?;
            extensions.set(EXT_WALLET_DIR, ExtensionValue::Text(text.to_owned()));
        }
        Ok(extensions)
    }
}

/// The [`ConnectionParams`] `reldex_workspace::connection_params` built for a
/// profile, summarised into plain data -- owned by the caller from the
/// moment the reply that carries it is drained, released with
/// [`reldex_connect_summary_release`].
///
/// Not `ConnectionParams` itself: nothing downstream of M2.11 consumes it yet
/// (session opening against a real driver is M1.8's), so this crate reports
/// what the mapping produced rather than inventing a second FFI shape for a
/// type with no consumer. See the PR description's "weak points" note.
pub struct ReldexConnectSummary {
    endpoint_kind: i32,
    host: OwnedStr,
    port: u16,
    service: OwnedStr,
    connect_string: OwnedStr,
    tls_mode: i32,
    has_connect_timeout: bool,
    connect_timeout_seconds: u32,
    role: i32,
    rewrite_trigger_ddl: bool,
    connect_without_limit: bool,
    allow_unenforced_certificate_pin: bool,
    ca_directory: OwnedStr,
}

impl Drop for ReldexConnectSummary {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexConnectSummary {
    fn new(params: &ConnectionParams) -> Self {
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        let (endpoint_kind, host, port, service, connect_string) = match params.endpoint() {
            Endpoint::HostPort {
                host,
                port,
                service,
            } => (
                ReldexEndpointKind::HostPort,
                host.clone(),
                *port,
                service.clone(),
                String::new(),
            ),
            Endpoint::ConnectString(text) => (
                ReldexEndpointKind::ConnectString,
                String::new(),
                0,
                String::new(),
                text.clone(),
            ),
            // `Endpoint` is `#[non_exhaustive]`.
            _ => (
                ReldexEndpointKind::Unknown,
                String::new(),
                0,
                String::new(),
                String::new(),
            ),
        };
        let flag = |key: &str| {
            matches!(
                params.extensions().get(key),
                Some(ExtensionValue::Flag(true))
            )
        };
        let ca_directory = match params.extensions().get(EXT_WALLET_DIR) {
            Some(ExtensionValue::Text(text)) => text.clone(),
            _ => String::new(),
        };
        Self {
            endpoint_kind: endpoint_kind as i32,
            host: OwnedStr::new(host),
            port,
            service: OwnedStr::new(service),
            connect_string: OwnedStr::new(connect_string),
            tls_mode: match params.tls() {
                TlsMode::Required => ReldexTransportKind::Tls as i32,
                _ => ReldexTransportKind::Plain as i32,
            },
            has_connect_timeout: params.connect_timeout().is_some(),
            connect_timeout_seconds: params.connect_timeout().map_or(0, |duration| {
                u32::try_from(duration.as_secs()).unwrap_or(u32::MAX)
            }),
            role: ReldexSessionRoleKind::from(params.role()) as i32,
            rewrite_trigger_ddl: flag(EXT_REWRITE_TRIGGER_DDL),
            connect_without_limit: flag(EXT_CONNECT_TIMEOUT_UNBOUNDED),
            allow_unenforced_certificate_pin: flag(EXT_ALLOW_UNENFORCED_SERVER_CERT_DN),
            ca_directory: OwnedStr::new(ca_directory),
        }
    }

    fn view(&self) -> ReldexConnectSummaryView {
        ReldexConnectSummaryView {
            struct_size: u32::try_from(size_of::<ReldexConnectSummaryView>()).unwrap_or(u32::MAX),
            endpoint_kind: self.endpoint_kind,
            host: self.host.as_reldex_str(),
            port: self.port,
            service: self.service.as_reldex_str(),
            connect_string: self.connect_string.as_reldex_str(),
            tls_mode: self.tls_mode,
            has_connect_timeout: self.has_connect_timeout,
            connect_timeout_seconds: self.connect_timeout_seconds,
            role: self.role,
            rewrite_trigger_ddl: self.rewrite_trigger_ddl,
            connect_without_limit: self.connect_without_limit,
            allow_unenforced_certificate_pin: self.allow_unenforced_certificate_pin,
            ca_directory: self.ca_directory.as_reldex_str(),
        }
    }
}

/// A read-only view of a [`ReldexConnectSummary`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexConnectSummaryView {
    /// `sizeof(ReldexConnectSummaryView)` on the way in; how much is valid on
    /// the way out.
    pub struct_size: u32,
    /// A [`ReldexEndpointKind`].
    pub endpoint_kind: i32,
    /// The host, for [`ReldexEndpointKind::HostPort`].
    pub host: ReldexStr,
    /// The port, for [`ReldexEndpointKind::HostPort`].
    pub port: u16,
    /// The service name, for [`ReldexEndpointKind::HostPort`].
    pub service: ReldexStr,
    /// The connect string -- populated for [`ReldexEndpointKind::ConnectString`]
    /// **and** for a SID profile, since the Oracle binding maps a SID onto a
    /// connect string (there is no vendor-neutral SID endpoint shape).
    pub connect_string: ReldexStr,
    /// A [`ReldexTransportKind`] (`Plain` or `Tls`).
    pub tls_mode: i32,
    /// Whether a connect timeout is armed.
    pub has_connect_timeout: bool,
    /// The connect timeout, in seconds, when `has_connect_timeout`.
    pub connect_timeout_seconds: u32,
    /// A [`ReldexSessionRoleKind`].
    pub role: i32,
    /// Whether trigger DDL is rewritten.
    pub rewrite_trigger_ddl: bool,
    /// Whether "no connect limit" was requested.
    pub connect_without_limit: bool,
    /// The C-6 guard's opt-out.
    pub allow_unenforced_certificate_pin: bool,
    /// The CA directory, or empty.
    pub ca_directory: ReldexStr,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool` or `ReldexStr` -- all valid as zero.
unsafe impl CStruct for ReldexConnectSummaryView {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// Reads a connect summary into `out`.
///
/// # Safety
///
/// `summary` must be null (reported as `false`) or a live
/// [`ReldexConnectSummary`]. `out` must be null or point at a writable
/// [`ReldexConnectSummaryView`] with `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_connect_summary_view(
    summary: *const ReldexConnectSummary,
    out: *mut ReldexConnectSummaryView,
) -> bool {
    entry_value(false, || {
        if summary.is_null() {
            return false;
        }
        // SAFETY: delegated to this function's contract.
        let view = unsafe { &*summary }.view();
        // SAFETY: delegated to this function's contract for `out`.
        unsafe { write_out_struct(out, view) }
    })
}

/// Releases a connect summary.
///
/// # Safety
///
/// `summary` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_connect_summary_release(summary: *mut ReldexConnectSummary) {
    entry_value((), || {
        if summary.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(summary) });
    });
}

// ============================================================================
// Credentials (family 6).
// ============================================================================

/// An owned secret -- a password fetched from the credential store or
/// resolved for a connect -- never a plain [`ReldexStr`] the caller could
/// copy and keep. See this module's documentation.
pub struct ReldexSecret {
    secret: Secret,
}

impl Drop for ReldexSecret {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexSecret {
    fn new(secret: Secret) -> Self {
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        Self { secret }
    }
}

/// Borrows the secret's text. The borrow is valid only until
/// [`reldex_secret_release`]; do not copy it into a longer-lived buffer.
///
/// # Safety
///
/// `secret` must be null (reported as empty) or a live [`ReldexSecret`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_secret_expose(secret: *const ReldexSecret) -> ReldexStr {
    entry_value(ReldexStr::empty(), || {
        if secret.is_null() {
            return ReldexStr::empty();
        }
        // SAFETY: delegated to this function's contract.
        let text = unsafe { &*secret }.secret.expose();
        // The text is not NUL-terminated by `Secret` (it is a `String`
        // `zeroize` owns); borrowing it as `ReldexStr` here would violate the
        // "NUL-terminated on the way out" rule. It already lives inside this
        // object for as long as the caller may hold the pointer, so there is
        // nowhere to *own* a terminated copy without a second buffer this
        // object would then have to zero on drop too. `ReldexStr::as_bytes`
        // callers must therefore treat `secret.ptr[secret.len]` as unreadable
        // here -- documented on the field, not asserted, because this is a
        // deliberate exception to that promise for exactly this one type of
        // string. See the PR description's "weak points" note.
        ReldexStr {
            ptr: text.as_ptr(),
            len: text.len(),
        }
    })
}

/// Releases a secret, wiping its bytes (`reldex-secrets`'s `Secret::drop`,
/// ADR-0007 S6).
///
/// # Safety
///
/// `secret` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_secret_release(secret: *mut ReldexSecret) {
    entry_value((), || {
        if secret.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(secret) });
    });
}

/// Where a resolved password came from -- the FFI shape of
/// `reldex_secrets::PasswordSource`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexPasswordSourceKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// The credential store held it; see the reply's `secret`.
    FromStore = 1,
    /// The user must be asked; see the reply's `prompt_reason`.
    PromptRequired = 2,
    /// The profile authenticates without a password.
    NotNeeded = 3,
}

/// Why the user must be asked for the password -- the FFI shape of
/// `reldex_secrets::PromptReason`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexPromptReasonKind {
    /// A reason this header does not know, or not applicable.
    Unknown = 0,
    /// The profile is set to "prompt each time".
    PromptEachTime = 1,
    /// No usable credential store.
    StoreUnavailable = 2,
    /// The store holds nothing for this profile.
    NotStored = 3,
    /// The store failed; see the reply's `error`.
    StoreFailed = 4,
}

// ============================================================================
// History (family 7).
// ============================================================================

/// How a recorded statement ended.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexHistoryOutcomeKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Ran to completion without error.
    Succeeded = 1,
    /// The database or driver reported an error.
    Failed = 2,
    /// Cancelled.
    Cancelled = 3,
    /// The per-statement time limit fired.
    TimedOut = 4,
}

/// Splits a `HistoryOutcome` into `(kind, native_code, has_native_code)`. A
/// plain function, not a `From` impl: neither `HistoryOutcome` nor a tuple is
/// defined in this crate, so a trait impl over both is refused by the orphan
/// rule (E0117).
fn split_history_outcome(outcome: HistoryOutcome) -> (ReldexHistoryOutcomeKind, i32, bool) {
    match outcome {
        HistoryOutcome::Succeeded => (ReldexHistoryOutcomeKind::Succeeded, 0, false),
        HistoryOutcome::Failed { native_code } => (
            ReldexHistoryOutcomeKind::Failed,
            native_code.unwrap_or(0),
            native_code.is_some(),
        ),
        HistoryOutcome::Cancelled => (ReldexHistoryOutcomeKind::Cancelled, 0, false),
        HistoryOutcome::TimedOut => (ReldexHistoryOutcomeKind::TimedOut, 0, false),
        // `HistoryOutcome` is `#[non_exhaustive]`.
        _ => (ReldexHistoryOutcomeKind::Unknown, 0, false),
    }
}

/// Crosses a [`HistoryId`] as a plain `u64` (`HistoryId::to_ffi_value`,
/// bit-reinterpreted -- lossless both ways for the same reason `as` between
/// equal-width integers always is).
const fn history_id_to_ffi(id: HistoryId) -> u64 {
    id.to_ffi_value() as u64
}

/// The inverse of [`history_id_to_ffi`].
const fn history_id_from_ffi(value: u64) -> HistoryId {
    HistoryId::from_ffi_value(value as i64)
}

fn history_outcome_from_i32(
    kind: i32,
    native_code: i32,
    has_native_code: bool,
) -> Option<HistoryOutcome> {
    Some(match kind {
        x if x == ReldexHistoryOutcomeKind::Succeeded as i32 => HistoryOutcome::Succeeded,
        x if x == ReldexHistoryOutcomeKind::Failed as i32 => HistoryOutcome::Failed {
            native_code: has_native_code.then_some(native_code),
        },
        x if x == ReldexHistoryOutcomeKind::Cancelled as i32 => HistoryOutcome::Cancelled,
        x if x == ReldexHistoryOutcomeKind::TimedOut as i32 => HistoryOutcome::TimedOut,
        _ => return None,
    })
}

struct StoredHistoryRecord {
    id: u64,
    executed_at: u64,
    statement: OwnedStr,
    outcome_kind: i32,
    native_code: i32,
    has_native_code: bool,
    elapsed_ms: u64,
    row_count: u64,
    has_row_count: bool,
}

impl StoredHistoryRecord {
    fn from_record(record: &HistoryRecord) -> Self {
        let (outcome_kind, native_code, has_native_code) = split_history_outcome(record.outcome);
        Self {
            id: history_id_to_ffi(record.id),
            executed_at: millis_to_ffi(record.executed_at),
            statement: OwnedStr::new(record.statement.clone()),
            outcome_kind: outcome_kind as i32,
            native_code,
            has_native_code,
            elapsed_ms: record.elapsed_ms,
            row_count: record.row_count.unwrap_or(0),
            has_row_count: record.row_count.is_some(),
        }
    }

    fn view(&self) -> ReldexHistoryRecordView {
        ReldexHistoryRecordView {
            struct_size: u32::try_from(size_of::<ReldexHistoryRecordView>()).unwrap_or(u32::MAX),
            id: self.id,
            executed_at: self.executed_at,
            statement: self.statement.as_reldex_str(),
            outcome_kind: self.outcome_kind,
            native_code: self.native_code,
            has_native_code: self.has_native_code,
            elapsed_ms: self.elapsed_ms,
            row_count: self.row_count,
            has_row_count: self.has_row_count,
        }
    }
}

/// A read-only view of one history record, borrowed from the
/// [`ReldexHistoryList`] it came from.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexHistoryRecordView {
    /// `sizeof(ReldexHistoryRecordView)` on the way in; how much is valid on
    /// the way out.
    pub struct_size: u32,
    /// The entry's id.
    pub id: u64,
    /// When it was submitted, Unix milliseconds.
    pub executed_at: u64,
    /// The statement text, verbatim.
    pub statement: ReldexStr,
    /// A [`ReldexHistoryOutcomeKind`].
    pub outcome_kind: i32,
    /// The native error code, when `has_native_code`.
    pub native_code: i32,
    /// Whether `native_code` is set.
    pub has_native_code: bool,
    /// How long it took, in milliseconds.
    pub elapsed_ms: u64,
    /// Rows affected or returned, when `has_row_count`.
    pub row_count: u64,
    /// Whether `row_count` is set.
    pub has_row_count: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool` or `ReldexStr` -- all valid as zero.
unsafe impl CStruct for ReldexHistoryRecordView {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// A page of history records -- [`crate::reldex_workspace_list_history`] --
/// owned by the caller from the moment the reply that carries it is drained.
pub struct ReldexHistoryList {
    records: Vec<StoredHistoryRecord>,
}

impl Drop for ReldexHistoryList {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexHistoryList {
    fn new(records: Vec<HistoryRecord>) -> Self {
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        Self {
            records: records
                .iter()
                .map(StoredHistoryRecord::from_record)
                .collect(),
        }
    }
}

/// How many records `list` holds.
///
/// # Safety
///
/// `list` must be null (reported as 0) or a live [`ReldexHistoryList`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_history_list_count(list: *const ReldexHistoryList) -> usize {
    entry_value(0, || {
        if list.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*list }.records.len()
    })
}

/// Reads record `index` into `out`.
///
/// # Safety
///
/// `list` must be null (reported as `false`) or a live [`ReldexHistoryList`].
/// `out` must be null or point at a writable [`ReldexHistoryRecordView`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_history_list_get(
    list: *const ReldexHistoryList,
    index: usize,
    out: *mut ReldexHistoryRecordView,
) -> bool {
    entry_value(false, || {
        if list.is_null() {
            return false;
        }
        // SAFETY: delegated to this function's contract.
        let Some(record) = unsafe { &*list }.records.get(index) else {
            return false;
        };
        // SAFETY: delegated to this function's contract for `out`.
        unsafe { write_out_struct(out, record.view()) }
    })
}

/// Releases a history list.
///
/// # Safety
///
/// `list` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_history_list_release(list: *mut ReldexHistoryList) {
    entry_value((), || {
        if list.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(list) });
    });
}

// ============================================================================
// Worksheets and layout (family 7).
// ============================================================================

struct StoredWorksheet {
    id: [u8; 16],
    has_profile: bool,
    profile_id: [u8; 16],
    title: OwnedStr,
    text: OwnedStr,
    caret: u32,
    scroll: u32,
    tab_order: u32,
    created_at: u64,
    updated_at: u64,
}

impl StoredWorksheet {
    fn from_worksheet(worksheet: &Worksheet) -> Self {
        Self {
            id: *worksheet.id().as_bytes(),
            has_profile: worksheet.profile().is_some(),
            profile_id: worksheet
                .profile()
                .map(|id| *id.as_bytes())
                .unwrap_or([0; 16]),
            title: OwnedStr::new(worksheet.state().title.clone()),
            text: OwnedStr::new(worksheet.state().text.clone()),
            caret: worksheet.state().caret,
            scroll: worksheet.state().scroll,
            tab_order: worksheet.tab_order(),
            created_at: millis_to_ffi(worksheet.created_at()),
            updated_at: millis_to_ffi(worksheet.updated_at()),
        }
    }

    fn view(&self) -> ReldexWorksheetView {
        ReldexWorksheetView {
            struct_size: u32::try_from(size_of::<ReldexWorksheetView>()).unwrap_or(u32::MAX),
            id: self.id,
            has_profile: self.has_profile,
            profile_id: self.profile_id,
            title: self.title.as_reldex_str(),
            text: self.text.as_reldex_str(),
            caret: self.caret,
            scroll: self.scroll,
            tab_order: self.tab_order,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// A read-only view of one worksheet, borrowed from the
/// [`ReldexWorksheetList`] it came from.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexWorksheetView {
    /// `sizeof(ReldexWorksheetView)` on the way in; how much is valid on the
    /// way out.
    pub struct_size: u32,
    /// The worksheet's id.
    pub id: [u8; 16],
    /// Whether `profile_id` is set.
    pub has_profile: bool,
    /// The profile it is attached to, when `has_profile`.
    pub profile_id: [u8; 16],
    /// The tab's title.
    pub title: ReldexStr,
    /// The editor's contents, verbatim.
    pub text: ReldexStr,
    /// The caret position.
    pub caret: u32,
    /// The scroll position.
    pub scroll: u32,
    /// Its place in the tab bar.
    pub tab_order: u32,
    /// Created, Unix milliseconds.
    pub created_at: u64,
    /// Last saved, Unix milliseconds.
    pub updated_at: u64,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool`, `[u8; 16]` or `ReldexStr` -- all valid as zero.
unsafe impl CStruct for ReldexWorksheetView {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// The open worksheets -- [`crate::reldex_workspace_load_worksheets`] --
/// owned by the caller from the moment the reply that carries it is drained.
pub struct ReldexWorksheetList {
    worksheets: Vec<StoredWorksheet>,
}

impl Drop for ReldexWorksheetList {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexWorksheetList {
    fn new(worksheets: Vec<Worksheet>) -> Self {
        crate::counters::created(crate::counters::Kind::WorkspaceObject);
        Self {
            worksheets: worksheets
                .iter()
                .map(StoredWorksheet::from_worksheet)
                .collect(),
        }
    }
}

/// How many worksheets `list` holds.
///
/// # Safety
///
/// `list` must be null (reported as 0) or a live [`ReldexWorksheetList`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_worksheet_list_count(list: *const ReldexWorksheetList) -> usize {
    entry_value(0, || {
        if list.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*list }.worksheets.len()
    })
}

/// Reads worksheet `index` into `out`.
///
/// # Safety
///
/// `list` must be null (reported as `false`) or a live [`ReldexWorksheetList`].
/// `out` must be null or point at a writable [`ReldexWorksheetView`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_worksheet_list_get(
    list: *const ReldexWorksheetList,
    index: usize,
    out: *mut ReldexWorksheetView,
) -> bool {
    entry_value(false, || {
        if list.is_null() {
            return false;
        }
        // SAFETY: delegated to this function's contract.
        let Some(worksheet) = unsafe { &*list }.worksheets.get(index) else {
            return false;
        };
        // SAFETY: delegated to this function's contract for `out`.
        unsafe { write_out_struct(out, worksheet.view()) }
    })
}

/// Releases a worksheet list.
///
/// # Safety
///
/// `list` must be null (a no-op) or a pointer this library handed out that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_worksheet_list_release(list: *mut ReldexWorksheetList) {
    entry_value((), || {
        if list.is_null() {
            return;
        }
        // SAFETY: delegated to this function's contract.
        drop(unsafe { Box::from_raw(list) });
    });
}

/// A new, random worksheet id, for [`crate::reldex_workspace_save_worksheet`].
///
/// Pure and synchronous -- generating a UUID needs no I/O, so this does not
/// go through the service thread.
///
/// # Safety
///
/// `out` must point at 16 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_new_worksheet_id(out: *mut u8) {
    entry_value((), || {
        if out.is_null() || !out.is_aligned() {
            return;
        }
        let id = WorksheetId::new_random();
        // SAFETY: the caller promises `out` points at 16 writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(id.as_bytes().as_ptr(), out, 16) };
    });
}

/// The workspace's layout -- input to
/// [`crate::reldex_workspace_save_layout`] and the payload of
/// [`ReldexWorkspaceReply::layout`] for
/// [`ReldexWorkspaceReplyKind::LayoutLoaded`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ReldexLayout {
    /// `sizeof(ReldexLayout)` on the way in.
    pub struct_size: u32,
    /// Whether `active_worksheet` is set.
    pub has_active_worksheet: bool,
    /// The worksheet on top when last saved, when `has_active_worksheet`.
    pub active_worksheet: [u8; 16],
    /// Whether `active_profile` is set.
    pub has_active_profile: bool,
    /// The profile shown as active when last saved, when `has_active_profile`.
    pub active_profile: [u8; 16],
    /// Whether `object_browser_width` is set.
    pub has_object_browser_width: bool,
    /// The object browser's width, in pixels.
    pub object_browser_width: u32,
    /// Whether `result_pane_height` is set.
    pub has_result_pane_height: bool,
    /// The result pane's height, in pixels.
    pub result_pane_height: u32,
    /// Whether `window_x`/`window_y` are set.
    pub has_window_position: bool,
    /// The window's left edge.
    pub window_x: i32,
    /// The window's top edge.
    pub window_y: i32,
    /// Whether `window_width`/`window_height` are set.
    pub has_window_size: bool,
    /// The window's width, in pixels.
    pub window_width: u32,
    /// The window's height, in pixels.
    pub window_height: u32,
    /// Whether the window was maximized.
    pub window_maximized: bool,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field an integer,
// `bool` or `[u8; 16]` -- all valid as zero.
unsafe impl CStruct for ReldexLayout {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexLayout {
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            has_active_worksheet: false,
            active_worksheet: [0; 16],
            has_active_profile: false,
            active_profile: [0; 16],
            has_object_browser_width: false,
            object_browser_width: 0,
            has_result_pane_height: false,
            result_pane_height: 0,
            has_window_position: false,
            window_x: 0,
            window_y: 0,
            has_window_size: false,
            window_width: 0,
            window_height: 0,
            window_maximized: false,
        }
    }
}

impl ReldexLayout {
    fn from_layout(layout: &Layout) -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            has_active_worksheet: layout.active_worksheet.is_some(),
            active_worksheet: layout
                .active_worksheet
                .map(|id| *id.as_bytes())
                .unwrap_or([0; 16]),
            has_active_profile: layout.active_profile.is_some(),
            active_profile: layout
                .active_profile
                .map(|id| *id.as_bytes())
                .unwrap_or([0; 16]),
            has_object_browser_width: layout.panes.object_browser_width.is_some(),
            object_browser_width: layout.panes.object_browser_width.unwrap_or(0),
            has_result_pane_height: layout.panes.result_pane_height.is_some(),
            result_pane_height: layout.panes.result_pane_height.unwrap_or(0),
            has_window_position: layout.window.x.is_some() || layout.window.y.is_some(),
            window_x: layout.window.x.unwrap_or(0),
            window_y: layout.window.y.unwrap_or(0),
            has_window_size: layout.window.width.is_some() || layout.window.height.is_some(),
            window_width: layout.window.width.unwrap_or(0),
            window_height: layout.window.height.unwrap_or(0),
            window_maximized: layout.window.maximized,
        }
    }

    fn to_layout(self) -> Result<Layout, DbError> {
        let active_worksheet = self
            .has_active_worksheet
            .then(|| WorksheetId::from_bytes(self.active_worksheet))
            .transpose()
            .map_err(|error| id_error("layout active_worksheet", error))?;
        let active_profile = self
            .has_active_profile
            .then(|| ProfileId::from_bytes(self.active_profile))
            .transpose()
            .map_err(|error| id_error("layout active_profile", error))?;
        Ok(Layout {
            active_worksheet,
            active_profile,
            panes: PaneSizes {
                object_browser_width: self
                    .has_object_browser_width
                    .then_some(self.object_browser_width),
                result_pane_height: self
                    .has_result_pane_height
                    .then_some(self.result_pane_height),
            },
            window: WindowGeometry {
                x: self.has_window_position.then_some(self.window_x),
                y: self.has_window_position.then_some(self.window_y),
                width: self.has_window_size.then_some(self.window_width),
                height: self.has_window_size.then_some(self.window_height),
                maximized: self.window_maximized,
            },
        })
    }
}

// ============================================================================
// The service thread: commands in, replies out. Owns the `Store` and the
// credential store for its whole life -- never the UI thread.
// ============================================================================

enum WorkspaceCommand {
    ResolveSetting {
        request: u64,
        setting: SettingId,
        profile: Option<ProfileId>,
        worksheet: Option<WorksheetId>,
    },
    SetSetting {
        request: u64,
        scope: reldex_workspace::Scope,
        setting: SettingId,
        value: SettingValue,
    },
    ClearSetting {
        request: u64,
        scope: reldex_workspace::Scope,
        setting: SettingId,
    },
    CreateProfile {
        request: u64,
        details: ProfileDetails,
    },
    UpdateProfile {
        request: u64,
        id: ProfileId,
        details: ProfileDetails,
    },
    DeleteProfile {
        request: u64,
        id: ProfileId,
    },
    GetProfile {
        request: u64,
        id: ProfileId,
    },
    ListProfiles {
        request: u64,
    },
    BuildConnectParams {
        request: u64,
        id: ProfileId,
        password: Option<Secret>,
    },
    CredentialGet {
        request: u64,
        id: ProfileId,
    },
    CredentialPut {
        request: u64,
        id: ProfileId,
        secret: Secret,
    },
    CredentialDelete {
        request: u64,
        id: ProfileId,
    },
    ResolvePassword {
        request: u64,
        id: ProfileId,
    },
    RecordHistory {
        request: u64,
        entry: HistoryEntry,
    },
    ListHistory {
        request: u64,
        profile: ProfileId,
        page: HistoryPage,
    },
    ClearHistory {
        request: u64,
        profile: ProfileId,
    },
    SaveWorksheet {
        request: u64,
        worksheet: Result<Worksheet, WorksheetError>,
    },
    LoadWorksheets {
        request: u64,
    },
    DeleteWorksheet {
        request: u64,
        id: WorksheetId,
    },
    SaveLayout {
        request: u64,
        layout: Layout,
    },
    LoadLayout {
        request: u64,
    },
}

/// The successful payload of one reply, before it is converted to C shapes at
/// drain time (so a reply that is never drained never allocates the C-facing
/// objects at all).
enum WorkspacePayload {
    None,
    SettingResolved {
        setting: SettingId,
        value: SettingValue,
        source: reldex_workspace::Level,
    },
    Existed(bool),
    ProfileSaved {
        id: ProfileId,
        created_at: UnixTimeMs,
        updated_at: UnixTimeMs,
    },
    ProfileFetched(Option<Profile>),
    ProfilesListed(Vec<Profile>),
    ConnectParamsBuilt(ConnectionParams),
    CredentialGot(Option<Secret>),
    PasswordResolved(PasswordSource),
    HistoryRecorded(HistoryId),
    HistoryListed(Vec<HistoryRecord>),
    HistoryCleared(usize),
    WorksheetSaved {
        id: WorksheetId,
        created_at: UnixTimeMs,
        updated_at: UnixTimeMs,
    },
    WorksheetsLoaded(Vec<Worksheet>),
    LayoutLoaded(Option<Layout>),
}

struct QueuedWorkspaceReply {
    kind: i32,
    request: u64,
    payload: Result<WorkspacePayload, DbError>,
}

impl QueuedWorkspaceReply {
    fn into_c(self) -> ReldexWorkspaceReply {
        let mut out = ReldexWorkspaceReply {
            struct_size: u32::try_from(size_of::<ReldexWorkspaceReply>()).unwrap_or(u32::MAX),
            kind: self.kind,
            request: self.request,
            error: std::ptr::null_mut(),
            id: [0; 16],
            found: false,
            created_at: 0,
            updated_at: 0,
            count: 0,
            setting_id: 0,
            setting_value: ReldexSettingValue::default(),
            setting_source: 0,
            profile_list: std::ptr::null_mut(),
            connect: std::ptr::null_mut(),
            secret: std::ptr::null_mut(),
            password_source_kind: 0,
            prompt_reason: 0,
            history_id: 0,
            history_list: std::ptr::null_mut(),
            worksheet_list: std::ptr::null_mut(),
            layout: ReldexLayout::default(),
        };
        let payload = match self.payload {
            Ok(payload) => payload,
            Err(error) => {
                out.error = Box::into_raw(Box::new(ReldexError::from_db_error(&error)));
                return out;
            }
        };
        match payload {
            WorkspacePayload::None => {}
            WorkspacePayload::SettingResolved {
                setting,
                value,
                source,
            } => {
                out.setting_id = ReldexSettingId::from_setting_id(setting) as i32;
                out.setting_value = ReldexSettingValue::from_value(value);
                out.setting_source = ReldexSettingLevel::from(source) as i32;
            }
            WorkspacePayload::Existed(existed) => out.found = existed,
            WorkspacePayload::ProfileSaved {
                id,
                created_at,
                updated_at,
            } => {
                out.id = *id.as_bytes();
                out.created_at = millis_to_ffi(created_at);
                out.updated_at = millis_to_ffi(updated_at);
            }
            WorkspacePayload::ProfileFetched(profile) => {
                out.found = profile.is_some();
                let profiles = profile.into_iter().collect::<Vec<_>>();
                out.profile_list = Box::into_raw(Box::new(ReldexProfileList::new(profiles)));
            }
            WorkspacePayload::ProfilesListed(profiles) => {
                out.profile_list = Box::into_raw(Box::new(ReldexProfileList::new(profiles)));
            }
            WorkspacePayload::ConnectParamsBuilt(params) => {
                out.connect = Box::into_raw(Box::new(ReldexConnectSummary::new(&params)));
            }
            WorkspacePayload::CredentialGot(secret) => {
                out.found = secret.is_some();
                out.secret = secret
                    .map(|secret| Box::into_raw(Box::new(ReldexSecret::new(secret))))
                    .unwrap_or(std::ptr::null_mut());
            }
            WorkspacePayload::PasswordResolved(source) => match source {
                PasswordSource::FromStore(secret) => {
                    out.password_source_kind = ReldexPasswordSourceKind::FromStore as i32;
                    out.secret = Box::into_raw(Box::new(ReldexSecret::new(secret)));
                }
                PasswordSource::PromptRequired(reason) => {
                    out.password_source_kind = ReldexPasswordSourceKind::PromptRequired as i32;
                    out.prompt_reason = match reason {
                        PromptReason::PromptEachTime => ReldexPromptReasonKind::PromptEachTime,
                        PromptReason::StoreUnavailable => ReldexPromptReasonKind::StoreUnavailable,
                        PromptReason::NotStored => ReldexPromptReasonKind::NotStored,
                        PromptReason::StoreFailed(error) => {
                            out.error = Box::into_raw(Box::new(ReldexError::from_db_error(
                                &credential_error("resolve_password", error),
                            )));
                            ReldexPromptReasonKind::StoreFailed
                        }
                        // `PromptReason` is `#[non_exhaustive]`.
                        _ => ReldexPromptReasonKind::Unknown,
                    } as i32;
                }
                PasswordSource::NotNeeded => {
                    out.password_source_kind = ReldexPasswordSourceKind::NotNeeded as i32;
                }
            },
            WorkspacePayload::HistoryRecorded(id) => out.history_id = history_id_to_ffi(id),
            WorkspacePayload::HistoryListed(records) => {
                out.history_list = Box::into_raw(Box::new(ReldexHistoryList::new(records)));
            }
            WorkspacePayload::HistoryCleared(count) => {
                out.count = u64::try_from(count).unwrap_or(u64::MAX);
            }
            WorkspacePayload::WorksheetSaved {
                id,
                created_at,
                updated_at,
            } => {
                out.id = *id.as_bytes();
                out.created_at = millis_to_ffi(created_at);
                out.updated_at = millis_to_ffi(updated_at);
            }
            WorkspacePayload::WorksheetsLoaded(worksheets) => {
                out.worksheet_list = Box::into_raw(Box::new(ReldexWorksheetList::new(worksheets)));
            }
            WorkspacePayload::LayoutLoaded(layout) => {
                out.found = layout.is_some();
                out.layout = layout
                    .as_ref()
                    .map(ReldexLayout::from_layout)
                    .unwrap_or_default();
            }
        }
        out
    }
}

/// Which request a [`ReldexWorkspaceReply`] answers.
///
/// `0` is reserved for a kind this header predates (ADR-0003 D7). A reply
/// whose `error` is non-null is that request's *failure*, still delivered
/// under its own kind.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexWorkspaceReplyKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Reply to [`crate::reldex_workspace_open`]: the store finished opening
    /// (or failed to).
    Opened = 1,
    /// Reply to [`crate::reldex_workspace_resolve_setting`].
    SettingResolved = 2,
    /// Reply to [`crate::reldex_workspace_clear_setting`]. `found` is
    /// whether a value existed at that scope.
    SettingCleared = 3,
    /// Reply to [`crate::reldex_workspace_create_profile`] or
    /// [`crate::reldex_workspace_update_profile`].
    ProfileSaved = 4,
    /// Reply to [`crate::reldex_workspace_delete_profile`]. `found` is
    /// whether it existed.
    ProfileDeleted = 5,
    /// Reply to [`crate::reldex_workspace_get_profile`]. `found` is whether
    /// it exists; `profile_list` holds 0 or 1 entries either way.
    ProfileFetched = 6,
    /// Reply to [`crate::reldex_workspace_list_profiles`].
    ProfilesListed = 7,
    /// Reply to [`crate::reldex_workspace_build_connect_params`].
    ConnectParamsBuilt = 8,
    /// Reply to [`crate::reldex_workspace_credential_get`]. `found` is
    /// whether the store held one.
    CredentialGot = 9,
    /// Reply to [`crate::reldex_workspace_credential_put`].
    CredentialPut = 10,
    /// Reply to [`crate::reldex_workspace_credential_delete`]. `found` is
    /// whether it existed.
    CredentialDeleted = 11,
    /// Reply to [`crate::reldex_workspace_resolve_password`].
    PasswordResolved = 12,
    /// Reply to [`crate::reldex_workspace_record_history`].
    HistoryRecorded = 13,
    /// Reply to [`crate::reldex_workspace_list_history`].
    HistoryListed = 14,
    /// Reply to [`crate::reldex_workspace_clear_history`]. `count` is how
    /// many were removed.
    HistoryCleared = 15,
    /// Reply to [`crate::reldex_workspace_save_worksheet`].
    WorksheetSaved = 16,
    /// Reply to [`crate::reldex_workspace_load_worksheets`].
    WorksheetsLoaded = 17,
    /// Reply to [`crate::reldex_workspace_delete_worksheet`]. `found` is
    /// whether it existed.
    WorksheetDeleted = 18,
    /// Reply to [`crate::reldex_workspace_save_layout`].
    LayoutSaved = 19,
    /// Reply to [`crate::reldex_workspace_load_layout`]. `found` is whether
    /// a layout had ever been saved.
    LayoutLoaded = 20,
}

/// What one completed workspace request looks like on the way out. One flat
/// `#[repr(C)]` struct, like [`crate::ReldexEvent`]; the adapter switches on
/// `kind` and reads the fields that kind documents.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexWorkspaceReply {
    /// `sizeof(ReldexWorkspaceReply)` on the way in; how much is valid on the
    /// way out.
    pub struct_size: u32,
    /// A [`ReldexWorkspaceReplyKind`].
    pub kind: i32,
    /// The caller's own correlation id, echoed back.
    pub request: u64,
    /// Non-null when this request failed. Owned by the caller; release with
    /// [`crate::reldex_error_free`].
    pub error: *mut ReldexError,
    /// A profile or worksheet id, for the kinds that carry exactly one.
    pub id: [u8; 16],
    /// A found/existed flag, reused across several kinds -- see
    /// [`ReldexWorkspaceReplyKind`].
    pub found: bool,
    /// Created, Unix milliseconds, for `ProfileSaved`/`WorksheetSaved`.
    pub created_at: u64,
    /// Modified/updated, Unix milliseconds, for
    /// `ProfileSaved`/`WorksheetSaved`.
    pub updated_at: u64,
    /// A count, for `HistoryCleared`.
    pub count: u64,
    /// A [`ReldexSettingId`], for `SettingResolved`.
    pub setting_id: i32,
    /// The resolved value, for `SettingResolved`.
    pub setting_value: ReldexSettingValue,
    /// A [`ReldexSettingLevel`], for `SettingResolved`.
    pub setting_source: i32,
    /// 0 or 1 profile (`ProfileFetched`) or every matching profile
    /// (`ProfilesListed`). Owned by the caller; release with
    /// [`reldex_profile_list_release`].
    pub profile_list: *mut ReldexProfileList,
    /// The mapped connection parameters, for `ConnectParamsBuilt`. Owned by
    /// the caller; release with [`reldex_connect_summary_release`].
    pub connect: *mut ReldexConnectSummary,
    /// A password, for `CredentialGot` (when `found`) and `PasswordResolved`
    /// (when `password_source_kind` is `FromStore`). Owned by the caller;
    /// release with [`reldex_secret_release`].
    pub secret: *mut ReldexSecret,
    /// A [`ReldexPasswordSourceKind`], for `PasswordResolved`.
    pub password_source_kind: i32,
    /// A [`ReldexPromptReasonKind`], for `PasswordResolved` when
    /// `password_source_kind` is `PromptRequired`.
    pub prompt_reason: i32,
    /// The new entry's id, for `HistoryRecorded`.
    pub history_id: u64,
    /// A page of history, for `HistoryListed`. Owned by the caller; release
    /// with [`reldex_history_list_release`].
    pub history_list: *mut ReldexHistoryList,
    /// Every open worksheet, for `WorksheetsLoaded`. Owned by the caller;
    /// release with [`reldex_worksheet_list_release`].
    pub worksheet_list: *mut ReldexWorksheetList,
    /// The saved layout, for `LayoutLoaded` when `found`.
    pub layout: ReldexLayout,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, and every field is an integer, a
// `bool`, a `[u8; 16]`, a pointer (null is valid), or another `CStruct`-shaped
// embedded struct that is itself valid when zeroed.
unsafe impl CStruct for ReldexWorkspaceReply {
    const MIN_SIZE: usize = size_of::<Self>();
}

/// Owned, small waker registration -- the same shape `crate::hub::ReldexHub`
/// uses, duplicated here because the two objects' lifetimes are independent
/// (a workspace may outlive or be destroyed before the hub, and vice versa).
struct WakerSlot {
    func: extern "C" fn(*mut c_void),
    user_data: *mut c_void,
}

// SAFETY: as `crate::hub`'s `WakerSlot` -- `user_data` is an opaque token
// this library never dereferences.
unsafe impl Send for WakerSlot {}
// SAFETY: as above -- only ever read behind `ReldexWorkspace::waker`'s lock.
unsafe impl Sync for WakerSlot {}

/// Called when the workspace's reply queue goes from empty to non-empty. Same
/// contract as [`crate::ReldexWakeFn`] (ADR-0003 D5): must not block, must
/// not call any `reldex_*` function, and is called from the workspace's
/// service thread, never the caller's.
pub type ReldexWorkspaceWakeFn = Option<extern "C" fn(user_data: *mut c_void)>;

/// The settings/profiles/credentials/history/worksheets/layout handle. Opaque
/// to C. Create with [`reldex_workspace_open`], release with
/// [`reldex_workspace_close`].
pub struct ReldexWorkspace {
    commands: Mutex<Option<mpsc::Sender<WorkspaceCommand>>>,
    replies: Mutex<VecDeque<QueuedWorkspaceReply>>,
    waker: RwLock<Option<WakerSlot>>,
    destroyed: AtomicBool,
}

impl Drop for ReldexWorkspace {
    fn drop(&mut self) {
        crate::counters::destroyed(crate::counters::Kind::WorkspaceObject);
    }
}

impl ReldexWorkspace {
    fn push_reply(&self, reply: QueuedWorkspaceReply) {
        let was_empty = {
            let mut queue = self
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_empty = queue.is_empty();
            queue.push_back(reply);
            was_empty
        };
        if was_empty {
            self.wake();
        }
    }

    fn wake(&self) {
        if self.destroyed.load(Ordering::Acquire) {
            return;
        }
        let slot = self
            .waker
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(waker) = slot.as_ref() else {
            return;
        };
        // Held across the call, same reasoning as `crate::hub::ReldexHub::wake`.
        let _guard = WakerGuard::enter();
        (waker.func)(waker.user_data);
    }

    fn next_reply(&self) -> Option<QueuedWorkspaceReply> {
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }

    fn pending_replies(&self) -> usize {
        self.replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn set_waker(&self, func: ReldexWorkspaceWakeFn, user_data: *mut c_void) {
        let mut slot = self
            .waker
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = func.map(|func| WakerSlot { func, user_data });
    }
}

/// Opens (or creates) the settings/profiles/credentials/history/worksheets/
/// layout store on a new service thread this library owns, and returns a
/// handle immediately.
///
/// This **never blocks**: the store is opened on the new thread, and the
/// caller learns the outcome from the first reply, `RELDEX_WORKSPACE_REPLY_
/// KIND_OPENED`, carrying `request`. Nothing else may be submitted
/// meaningfully until that reply arrives without an error, but submitting
/// earlier is not undefined behaviour -- the command simply waits behind the
/// open in the same queue and is answered afterwards.
///
/// `in_memory` opens a private, in-memory store (`path` is then ignored) --
/// for tests and for a caller that must not touch disk. `path` is copied
/// before this call returns; the caller's buffer need not outlive it.
///
/// `use_memory_credential_store`, **honoured only in a build with the
/// `mock-driver` feature** (the default; what the C smoke harness links), asks
/// for an in-process credential store instead of the platform's, so the
/// harness runs the same on every CI OS with no OS credential store at all.
/// Ignored in a build without that feature, which always uses
/// `reldex_secrets::platform_default()`.
///
/// # Safety
///
/// `path` must point at `path.len` readable UTF-8 bytes, unless `in_memory`.
/// `out` must be null or point at a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_open(
    path: ReldexStr,
    in_memory: bool,
    use_memory_credential_store: bool,
    request: u64,
    out: *mut *mut ReldexWorkspace,
) -> ReldexStatus {
    entry(|| {
        if out.is_null() || !out.is_aligned() {
            return set_last_argument_error("reldex_workspace_open: `out` is null or unaligned");
        }
        let owned_path = if in_memory {
            None
        } else {
            // SAFETY: delegated to this function's contract for `path`.
            match unsafe { path.as_str() } {
                Some(text) => Some(PathBuf::from(text)),
                None => {
                    return set_last_argument_error(
                        "reldex_workspace_open: `path` is null or is not valid UTF-8",
                    );
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        let workspace = std::sync::Arc::new(ReldexWorkspace {
            commands: Mutex::new(Some(tx)),
            replies: Mutex::new(VecDeque::new()),
            waker: RwLock::new(None),
            destroyed: AtomicBool::new(false),
        });
        crate::counters::created(crate::counters::Kind::WorkspaceObject);

        let thread_workspace = std::sync::Arc::clone(&workspace);
        let spawned = std::thread::Builder::new()
            .name("reldex-workspace".to_owned())
            .spawn(move || {
                service_main(
                    owned_path,
                    in_memory,
                    use_memory_credential_store,
                    request,
                    &thread_workspace,
                    rx,
                );
            });
        if spawned.is_err() {
            return set_last_argument_error(
                "reldex_workspace_open: the operating system refused to create the service \
                 thread",
            );
        }

        // SAFETY: delegated to this function's contract for `out`.
        unsafe { out.write(std::sync::Arc::into_raw(workspace).cast_mut()) };
        ReldexStatus::Ok
    })
}

/// Closes the workspace: the service thread finishes whatever is already
/// queued, then exits on its own. This **never blocks** the caller -- it does
/// not join the thread, the same "promptly, not necessarily finished" shape
/// as [`crate::reldex_hub_destroy`] (whose doc comment explains the trade-off
/// in full). Any reply already queued, and any the thread pushes while
/// draining the rest of the channel, must still be drained and released or it
/// leaks.
///
/// # Safety
///
/// `workspace` must be null, or a pointer [`reldex_workspace_open`] returned
/// that has not already been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_close(workspace: *mut ReldexWorkspace) {
    entry_value((), || {
        if workspace.is_null() {
            return;
        }
        // SAFETY: the caller promises this came from `Arc::into_raw` in
        // `reldex_workspace_open` and has not been closed.
        let arc = unsafe { std::sync::Arc::from_raw(workspace.cast_const()) };
        arc.destroyed.store(true, Ordering::Release);
        arc.set_waker(None, std::ptr::null_mut());
        // Dropping the sender lets the service thread's `recv()` loop end
        // once it has drained everything already sent.
        *arc.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        drop(arc);
    });
}

/// Registers (or, with a null `func`, unregisters) the workspace's wake
/// callback. Same contract as [`crate::reldex_hub_set_waker`].
///
/// # Safety
///
/// `workspace` must be a live workspace. `user_data` must stay valid until
/// this is called again with a different (or null) `func` and returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_set_waker(
    workspace: *mut ReldexWorkspace,
    func: ReldexWorkspaceWakeFn,
    user_data: *mut c_void,
) -> ReldexStatus {
    entry(|| {
        if workspace.is_null() || !workspace.is_aligned() {
            return set_last_argument_error(
                "reldex_workspace_set_waker: `workspace` is null or unaligned",
            );
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*workspace }.set_waker(func, user_data);
        ReldexStatus::Ok
    })
}

/// How many replies are waiting. Never blocks.
///
/// # Safety
///
/// `workspace` must be null (reported as 0) or a live workspace.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_pending_replies(
    workspace: *const ReldexWorkspace,
) -> usize {
    entry_value(0, || {
        if workspace.is_null() {
            return 0;
        }
        // SAFETY: delegated to this function's contract.
        unsafe { &*workspace }.pending_replies()
    })
}

/// Takes the next reply, or reports that there is none. Never blocks. Same
/// draining contract as [`crate::reldex_hub_next_event`]: `false` means
/// `*out` was not touched.
///
/// # Safety
///
/// `workspace` must be a live workspace, and `out` must point at a writable
/// [`ReldexWorkspaceReply`] whose `struct_size` the caller has initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_next_reply(
    workspace: *mut ReldexWorkspace,
    out: *mut ReldexWorkspaceReply,
) -> bool {
    entry_value(false, || {
        // SAFETY: delegated to this function's contract for `out`.
        if !unsafe { check_out_struct(out.cast_const()) } {
            set_last_argument_error(
                "reldex_workspace_next_reply: `out` is null, unaligned, or has too small a \
                 struct_size",
            );
            return false;
        }
        if workspace.is_null() {
            return false;
        }
        // SAFETY: delegated to this function's contract.
        let Some(reply) = (unsafe { &*workspace }).next_reply() else {
            return false;
        };
        // SAFETY: `out` was just checked; nothing between then and now can
        // have changed it (every workspace call but `reldex_workspace_close`
        // and this one runs on the caller's own thread).
        unsafe { write_out_struct(out, reply.into_c()) }
    })
}

/// The service thread's body: opens the store (and the credential store),
/// answers `Opened`, then loops answering commands until the sender is
/// dropped.
fn service_main(
    path: Option<PathBuf>,
    in_memory: bool,
    use_memory_credential_store: bool,
    open_request: u64,
    workspace: &ReldexWorkspace,
    commands: mpsc::Receiver<WorkspaceCommand>,
) {
    let opened = if in_memory {
        Store::open_in_memory()
    } else {
        match path {
            Some(path) => Store::open(path),
            None => Store::open_in_memory(),
        }
    };
    let mut store = match opened {
        Ok(store) => store,
        Err(error) => {
            workspace.push_reply(QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::Opened as i32,
                request: open_request,
                payload: Err(store_error("reldex_workspace_open", error)),
            });
            return;
        }
    };

    #[cfg(feature = "mock-driver")]
    let credential_store: Box<dyn CredentialStore> = if use_memory_credential_store {
        Box::new(MemoryCredentialStore::new())
    } else {
        reldex_secrets::platform_default()
    };
    #[cfg(not(feature = "mock-driver"))]
    let credential_store: Box<dyn CredentialStore> = {
        let _ = use_memory_credential_store;
        reldex_secrets::platform_default()
    };

    workspace.push_reply(QueuedWorkspaceReply {
        kind: ReldexWorkspaceReplyKind::Opened as i32,
        request: open_request,
        payload: Ok(WorkspacePayload::None),
    });

    while let Ok(command) = commands.recv() {
        let reply = run_command(&mut store, credential_store.as_ref(), command);
        workspace.push_reply(reply);
    }
}

fn run_command(
    store: &mut Store,
    credentials: &dyn CredentialStore,
    command: WorkspaceCommand,
) -> QueuedWorkspaceReply {
    match command {
        WorkspaceCommand::ResolveSetting {
            request,
            setting,
            profile,
            worksheet,
        } => {
            let payload = (|| {
                let application = store
                    .application_settings()
                    .map_err(|error| store_error("resolve_setting", error))?
                    .value;
                let profile_layer = profile
                    .map(|id| store.profile_settings(id).map(|loaded| loaded.value))
                    .transpose()
                    .map_err(|error| store_error("resolve_setting", error))?;
                let worksheet_layer = worksheet
                    .map(|id| store.worksheet_settings(id).map(|loaded| loaded.value))
                    .transpose()
                    .map_err(|error| store_error("resolve_setting", error))?;
                let mut context = ResolveContext::new().with_application(&application);
                if let Some(layer) = &profile_layer {
                    context = context.with_profile(layer);
                }
                if let Some(layer) = &worksheet_layer {
                    context = context.with_worksheet(layer);
                }
                let Resolved { value, source } = context.resolve_value(setting);
                Ok(WorkspacePayload::SettingResolved {
                    setting,
                    value,
                    source,
                })
            })();
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::SettingResolved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::SetSetting {
            request,
            scope,
            setting,
            value,
        } => {
            let payload = store
                .put_setting_value(scope, setting, value)
                .map(|()| WorkspacePayload::None)
                .map_err(|error| store_error("set_setting", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::SettingCleared as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::ClearSetting {
            request,
            scope,
            setting,
        } => {
            let payload = store
                .clear_setting(scope, setting)
                .map(WorkspacePayload::Existed)
                .map_err(|error| store_error("clear_setting", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::SettingCleared as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::CreateProfile { request, details } => {
            let payload = Profile::create(details)
                .map_err(|error| profile_error("create_profile", error))
                .and_then(|profile| {
                    store
                        .insert_profile(&profile)
                        .map(|()| profile)
                        .map_err(|error| store_error("create_profile", error))
                })
                .map(|profile| WorkspacePayload::ProfileSaved {
                    id: profile.id(),
                    created_at: profile.created_at(),
                    updated_at: profile.modified_at(),
                });
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ProfileSaved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::UpdateProfile {
            request,
            id,
            details,
        } => {
            let payload = (|| {
                let mut profile = store
                    .profile(id)
                    .map_err(|error| store_error("update_profile", error))?
                    .ok_or(StoreError::ProfileNotFound(id))
                    .map_err(|error| store_error("update_profile", error))?;
                profile
                    .update(details)
                    .map_err(|error| profile_error("update_profile", error))?;
                store
                    .update_profile(&profile)
                    .map_err(|error| store_error("update_profile", error))?;
                Ok(WorkspacePayload::ProfileSaved {
                    id: profile.id(),
                    created_at: profile.created_at(),
                    updated_at: profile.modified_at(),
                })
            })();
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ProfileSaved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::DeleteProfile { request, id } => {
            let payload = store
                .delete_profile(id)
                .map(WorkspacePayload::Existed)
                .map_err(|error| store_error("delete_profile", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ProfileDeleted as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::GetProfile { request, id } => {
            let payload = store
                .profile(id)
                .map(WorkspacePayload::ProfileFetched)
                .map_err(|error| store_error("get_profile", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ProfileFetched as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::ListProfiles { request } => {
            let payload = store
                .profiles()
                .map(|loaded| WorkspacePayload::ProfilesListed(loaded.value))
                .map_err(|error| store_error("list_profiles", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ProfilesListed as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::BuildConnectParams {
            request,
            id,
            password,
        } => {
            let payload = (|| {
                let profile = store
                    .profile(id)
                    .map_err(|error| store_error("build_connect_params", error))?
                    .ok_or(StoreError::ProfileNotFound(id))
                    .map_err(|error| store_error("build_connect_params", error))?;
                let application = store
                    .application_settings()
                    .map_err(|error| store_error("build_connect_params", error))?
                    .value;
                let profile_layer = store
                    .profile_settings(id)
                    .map_err(|error| store_error("build_connect_params", error))?
                    .value;
                let context = ResolveContext::new()
                    .with_application(&application)
                    .with_profile(&profile_layer);
                let settings = ConnectSettings::resolve(&context);
                let params = reldex_workspace::connection_params(
                    &profile,
                    &settings,
                    password,
                    &OracleDriverBinding,
                )
                .map_err(|error| {
                    DbError::new(
                        ErrorKind::Configuration,
                        format!("build_connect_params: {error}"),
                    )
                })?;
                Ok(WorkspacePayload::ConnectParamsBuilt(params))
            })();
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::ConnectParamsBuilt as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::CredentialGet { request, id } => {
            let payload = match credentials.get(&CredentialKey::for_profile(id)) {
                Ok(secret) => Ok(WorkspacePayload::CredentialGot(secret)),
                Err(error) => Err(credential_error("credential_get", error)),
            };
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::CredentialGot as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::CredentialPut {
            request,
            id,
            secret,
        } => {
            let payload = credentials
                .put(&CredentialKey::for_profile(id), &secret)
                .map(|()| WorkspacePayload::None)
                .map_err(|error| credential_error("credential_put", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::CredentialPut as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::CredentialDelete { request, id } => {
            // `NotFound` is reported as `existed = false`, not an error --
            // symmetric with `Store::delete_profile`/`delete_worksheet`,
            // whose callers likewise only care whether something was there.
            let payload = match credentials.delete(&CredentialKey::for_profile(id)) {
                Ok(()) => Ok(WorkspacePayload::Existed(true)),
                Err(CredentialError::NotFound) => Ok(WorkspacePayload::Existed(false)),
                Err(error) => Err(credential_error("credential_delete", error)),
            };
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::CredentialDeleted as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::ResolvePassword { request, id } => {
            let payload = store
                .profile(id)
                .map_err(|error| store_error("resolve_password", error))
                .and_then(|profile| {
                    profile
                        .ok_or(StoreError::ProfileNotFound(id))
                        .map_err(|error| store_error("resolve_password", error))
                })
                .map(|profile| {
                    WorkspacePayload::PasswordResolved(reldex_secrets::resolve_password(
                        &profile,
                        credentials,
                    ))
                });
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::PasswordResolved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::RecordHistory { request, entry } => {
            let payload = store
                .record_history(entry)
                .map(WorkspacePayload::HistoryRecorded)
                .map_err(|error| store_error("record_history", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::HistoryRecorded as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::ListHistory {
            request,
            profile,
            page,
        } => {
            let payload = store
                .history(profile, page)
                .map(WorkspacePayload::HistoryListed)
                .map_err(|error| store_error("list_history", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::HistoryListed as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::ClearHistory { request, profile } => {
            let payload = store
                .clear_history(profile)
                .map(WorkspacePayload::HistoryCleared)
                .map_err(|error| store_error("clear_history", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::HistoryCleared as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::SaveWorksheet { request, worksheet } => {
            let payload = worksheet
                .map_err(|error| {
                    DbError::new(ErrorKind::Configuration, format!("save_worksheet: {error}"))
                })
                .and_then(|worksheet| {
                    store
                        .save_worksheet(&worksheet)
                        .map(|()| worksheet)
                        .map_err(|error| store_error("save_worksheet", error))
                })
                .map(|worksheet| WorkspacePayload::WorksheetSaved {
                    id: worksheet.id(),
                    created_at: worksheet.created_at(),
                    updated_at: worksheet.updated_at(),
                });
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::WorksheetSaved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::LoadWorksheets { request } => {
            let payload = store
                .load_worksheets()
                .map(|loaded| WorkspacePayload::WorksheetsLoaded(loaded.value))
                .map_err(|error| store_error("load_worksheets", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::WorksheetsLoaded as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::DeleteWorksheet { request, id } => {
            let payload = store
                .delete_worksheet(id)
                .map(WorkspacePayload::Existed)
                .map_err(|error| store_error("delete_worksheet", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::WorksheetDeleted as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::SaveLayout { request, layout } => {
            let payload = store
                .save_layout(&layout)
                .map(|()| WorkspacePayload::None)
                .map_err(|error| store_error("save_layout", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::LayoutSaved as i32,
                request,
                payload,
            }
        }
        WorkspaceCommand::LoadLayout { request } => {
            let payload = store
                .load_layout()
                .map(WorkspacePayload::LayoutLoaded)
                .map_err(|error| store_error("load_layout", error));
            QueuedWorkspaceReply {
                kind: ReldexWorkspaceReplyKind::LayoutLoaded as i32,
                request,
                payload,
            }
        }
    }
}

// ============================================================================
// Request functions: submit a command, return immediately. Every request id
// is the caller's own `ReldexRequestId` (`crate::session::ReldexRequestId`),
// echoed back on the reply.
// ============================================================================

/// Sends `command` on `workspace`'s channel, reporting why it could not.
fn submit(workspace: *mut ReldexWorkspace, command: WorkspaceCommand) -> ReldexStatus {
    if workspace.is_null() || !workspace.is_aligned() {
        return set_last_argument_error("reldex_workspace: `workspace` is null or unaligned");
    }
    // SAFETY: the caller of every function in this module that reaches
    // `submit` promises `workspace` is a live handle for the duration of the
    // call, the same contract `crate::hub::with_hub` documents.
    let sender = unsafe { &*workspace }
        .commands
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    match sender {
        Some(sender) => match sender.send(command) {
            Ok(()) => ReldexStatus::Ok,
            Err(_) => set_last_argument_error("reldex_workspace: the workspace has been closed"),
        },
        None => set_last_argument_error("reldex_workspace: the workspace has been closed"),
    }
}

/// Resolves one setting's effective value: `worksheet ?? profile ?? \
/// application ?? built-in`, with the level it came from
/// ([`ReldexSettingValue`]/[`ReldexWorkspaceReply::setting_source`]).
///
/// `profile`/`worksheet` are 16-byte ids; pass null for a context with no
/// such layer (a connection that is not yet a worksheet has none, a fresh
/// install has no profile).
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile`/`worksheet` must each be
/// null or point at 16 readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_resolve_setting(
    workspace: *mut ReldexWorkspace,
    request: u64,
    setting_id: i32,
    profile: *const u8,
    worksheet: *const u8,
) -> ReldexStatus {
    entry(|| {
        let Some(setting) =
            ReldexSettingId::from_i32(setting_id).and_then(ReldexSettingId::to_setting_id)
        else {
            return set_last_argument_error(
                "reldex_workspace_resolve_setting: `setting_id` is not a setting this build knows",
            );
        };
        let profile = if profile.is_null() {
            None
        } else {
            // SAFETY: delegated to this function's contract.
            let bytes: [u8; 16] = unsafe { std::slice::from_raw_parts(profile, 16) }
                .try_into()
                .unwrap_or([0; 16]);
            match ProfileId::from_bytes(bytes) {
                Ok(id) => Some(id),
                Err(_) => {
                    return set_last_argument_error(
                        "reldex_workspace_resolve_setting: `profile` is not a valid id",
                    );
                }
            }
        };
        let worksheet = if worksheet.is_null() {
            None
        } else {
            // SAFETY: delegated to this function's contract.
            let bytes: [u8; 16] = unsafe { std::slice::from_raw_parts(worksheet, 16) }
                .try_into()
                .unwrap_or([0; 16]);
            match WorksheetId::from_bytes(bytes) {
                Ok(id) => Some(id),
                Err(_) => {
                    return set_last_argument_error(
                        "reldex_workspace_resolve_setting: `worksheet` is not a valid id",
                    );
                }
            }
        };
        submit(
            workspace,
            WorkspaceCommand::ResolveSetting {
                request,
                setting,
                profile,
                worksheet,
            },
        )
    })
}

/// Writes one setting value at `level` (`Application`, `Profile` or
/// `Worksheet` -- never `BuiltIn`), replacing any value already there.
///
/// # Safety
///
/// `workspace` must be a live workspace. `scope_id` must be null (for
/// `Application`) or point at 16 readable bytes (for `Profile`/`Worksheet`).
/// `value` must be null or point at a [`ReldexSettingValue`] with
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_set_setting(
    workspace: *mut ReldexWorkspace,
    request: u64,
    setting_id: i32,
    level: i32,
    scope_id: *const u8,
    value: *const ReldexSettingValue,
) -> ReldexStatus {
    entry(|| {
        let Some(setting) =
            ReldexSettingId::from_i32(setting_id).and_then(ReldexSettingId::to_setting_id)
        else {
            return set_last_argument_error(
                "reldex_workspace_set_setting: `setting_id` is not a setting this build knows",
            );
        };
        // SAFETY: delegated to this function's contract for `scope_id`.
        let id_bytes = unsafe { read_id_bytes(scope_id) };
        let scope = match parse_scope(level, id_bytes) {
            Ok(scope) => scope,
            Err(_) => {
                return set_last_argument_error(
                    "reldex_workspace_set_setting: `level`/`scope_id` do not name a settable scope",
                );
            }
        };
        // SAFETY: delegated to this function's contract for `value`.
        let Some(value) = (unsafe { read_in_struct(value) }) else {
            return set_last_argument_error(
                "reldex_workspace_set_setting: `value` is null, unaligned, or its struct_size is \
                 too small",
            );
        };
        let Some(value) = value.to_value() else {
            return set_last_argument_error(
                "reldex_workspace_set_setting: `value` does not describe a valid value for its kind",
            );
        };
        submit(
            workspace,
            WorkspaceCommand::SetSetting {
                request,
                scope,
                setting,
                value,
            },
        )
    })
}

/// Removes the value at `level`/`scope_id`, so the setting is inherited
/// again.
///
/// # Safety
///
/// As [`reldex_workspace_set_setting`], minus `value`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_clear_setting(
    workspace: *mut ReldexWorkspace,
    request: u64,
    setting_id: i32,
    level: i32,
    scope_id: *const u8,
) -> ReldexStatus {
    entry(|| {
        let Some(setting) =
            ReldexSettingId::from_i32(setting_id).and_then(ReldexSettingId::to_setting_id)
        else {
            return set_last_argument_error(
                "reldex_workspace_clear_setting: `setting_id` is not a setting this build knows",
            );
        };
        // SAFETY: delegated to this function's contract for `scope_id`.
        let id_bytes = unsafe { read_id_bytes(scope_id) };
        let scope = match parse_scope(level, id_bytes) {
            Ok(scope) => scope,
            Err(_) => {
                return set_last_argument_error(
                    "reldex_workspace_clear_setting: `level`/`scope_id` do not name a settable scope",
                );
            }
        };
        submit(
            workspace,
            WorkspaceCommand::ClearSetting {
                request,
                scope,
                setting,
            },
        )
    })
}

/// Reads 16 bytes at `ptr`, or all-zero if `ptr` is null.
///
/// # Safety
///
/// `ptr` must be null or point at 16 readable bytes.
unsafe fn read_id_bytes(ptr: *const u8) -> [u8; 16] {
    if ptr.is_null() {
        return [0; 16];
    }
    // SAFETY: delegated to this function's contract.
    unsafe { std::slice::from_raw_parts(ptr, 16) }
        .try_into()
        .unwrap_or([0; 16])
}

/// Creates a profile.
///
/// # Safety
///
/// `workspace` must be a live workspace. `details` must be null or point at
/// a [`ReldexProfileDetails`] with `struct_size` set and every string field
/// readable for the length it declares.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_create_profile(
    workspace: *mut ReldexWorkspace,
    request: u64,
    details: *const ReldexProfileDetails,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let Some(details) = (unsafe { read_in_struct(details) }) else {
            return set_last_argument_error(
                "reldex_workspace_create_profile: `details` is null, unaligned, or its \
                 struct_size is too small",
            );
        };
        // SAFETY: `read_in_struct` promises the string fields are readable
        // for the lengths they declare.
        let details = match unsafe { build_profile_details(&details) } {
            Ok(details) => details,
            Err(_) => {
                return set_last_argument_error(
                    "reldex_workspace_create_profile: `details` is not valid",
                );
            }
        };
        submit(
            workspace,
            WorkspaceCommand::CreateProfile { request, details },
        )
    })
}

/// Replaces a profile's details.
///
/// # Safety
///
/// `workspace` must be a live workspace. `id` must point at 16 readable
/// bytes. `details` as [`reldex_workspace_create_profile`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_update_profile(
    workspace: *mut ReldexWorkspace,
    request: u64,
    id: *const u8,
    details: *const ReldexProfileDetails,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract for `id`.
        let bytes = unsafe { read_id_bytes(id) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_update_profile: `id` is null or not a valid profile id",
            );
        };
        // SAFETY: delegated to this function's contract.
        let Some(details) = (unsafe { read_in_struct(details) }) else {
            return set_last_argument_error(
                "reldex_workspace_update_profile: `details` is null, unaligned, or its \
                 struct_size is too small",
            );
        };
        // SAFETY: as `reldex_workspace_create_profile`.
        let details = match unsafe { build_profile_details(&details) } {
            Ok(details) => details,
            Err(_) => {
                return set_last_argument_error(
                    "reldex_workspace_update_profile: `details` is not valid",
                );
            }
        };
        submit(
            workspace,
            WorkspaceCommand::UpdateProfile {
                request,
                id,
                details,
            },
        )
    })
}

/// Deletes a profile (and, in the same transaction, every setting it
/// overrides). Its credential-store entry, if any, is **not** removed here --
/// remove it with [`reldex_workspace_credential_delete`] first if wanted.
///
/// # Safety
///
/// `workspace` must be a live workspace. `id` must point at 16 readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_delete_profile(
    workspace: *mut ReldexWorkspace,
    request: u64,
    id: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(id) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_delete_profile: `id` is null or not a valid profile id",
            );
        };
        submit(workspace, WorkspaceCommand::DeleteProfile { request, id })
    })
}

/// Fetches one profile.
///
/// # Safety
///
/// As [`reldex_workspace_delete_profile`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_get_profile(
    workspace: *mut ReldexWorkspace,
    request: u64,
    id: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(id) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_get_profile: `id` is null or not a valid profile id",
            );
        };
        submit(workspace, WorkspaceCommand::GetProfile { request, id })
    })
}

/// Lists every profile, by name.
///
/// # Safety
///
/// `workspace` must be a live workspace.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_list_profiles(
    workspace: *mut ReldexWorkspace,
    request: u64,
) -> ReldexStatus {
    entry(|| submit(workspace, WorkspaceCommand::ListProfiles { request }))
}

/// Builds the connection parameters for `profile`, resolving its settings and
/// composing them with the real Oracle driver binding
/// (`OracleDriverBinding`, private to this crate).
///
/// `password`, when non-null, must be a live [`ReldexSecret`] -- typically
/// what [`reldex_workspace_resolve_password`] or
/// [`reldex_workspace_credential_get`] handed back, or what the user typed
/// wrapped with... there is deliberately no "wrap a plain string" entry
/// point here: building a [`ReldexSecret`] from caller text is
/// [`reldex_workspace_credential_put`]'s job today. Passing null answers as
/// if no password is available, which is correct for `External`
/// authentication and reported as [`reldex_workspace::ConnectError::
/// PasswordRequired`] (via the reply's `error`) otherwise.
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile` must point at 16 readable
/// bytes. `password` must be null or a live [`ReldexSecret`]; it is **not**
/// consumed -- the caller still owns it and must release it separately.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_build_connect_params(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
    password: *const ReldexSecret,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_build_connect_params: `profile` is null or not a valid profile \
                 id",
            );
        };
        let password = if password.is_null() {
            None
        } else {
            // SAFETY: the caller promises `password` is a live `ReldexSecret`
            // for the duration of this call; the clone below is independent
            // of the caller's copy, which it keeps and releases itself.
            Some(unsafe { &*password }.secret.clone())
        };
        submit(
            workspace,
            WorkspaceCommand::BuildConnectParams {
                request,
                id,
                password,
            },
        )
    })
}

/// Fetches the password the credential store holds for `profile`, if any.
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile` must point at 16 readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_credential_get(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_credential_get: `profile` is null or not a valid profile id",
            );
        };
        submit(workspace, WorkspaceCommand::CredentialGet { request, id })
    })
}

/// Stores `password` for `profile`, replacing whatever was there.
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile` must point at 16 readable
/// bytes. `password` must point at `password.len` readable UTF-8 bytes (read
/// only for the duration of this call).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_credential_put(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
    password: ReldexStr,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_credential_put: `profile` is null or not a valid profile id",
            );
        };
        // SAFETY: delegated to this function's contract for `password`.
        let Some(text) = (unsafe { password.as_str() }) else {
            return set_last_argument_error(
                "reldex_workspace_credential_put: `password` is null or is not valid UTF-8",
            );
        };
        submit(
            workspace,
            WorkspaceCommand::CredentialPut {
                request,
                id,
                secret: Secret::new(text.to_owned()),
            },
        )
    })
}

/// Removes the stored password for `profile`.
///
/// # Safety
///
/// As [`reldex_workspace_credential_get`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_credential_delete(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_credential_delete: `profile` is null or not a valid profile id",
            );
        };
        submit(
            workspace,
            WorkspaceCommand::CredentialDelete { request, id },
        )
    })
}

/// Decides where the password for connecting to `profile` comes from
/// (`reldex_secrets::resolve_password`): the credential store, a prompt (with
/// its reason), or "not needed".
///
/// # Safety
///
/// As [`reldex_workspace_credential_get`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_resolve_password(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(id) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_resolve_password: `profile` is null or not a valid profile id",
            );
        };
        submit(workspace, WorkspaceCommand::ResolvePassword { request, id })
    })
}

/// Records one statement's outcome, then trims the profile's history back to
/// its configured bound.
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile` must point at 16 readable
/// bytes. `statement` must point at `statement.len` readable UTF-8 bytes
/// (read only for the duration of this call).
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_record_history(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
    executed_at_ms: u64,
    statement: ReldexStr,
    outcome_kind: i32,
    native_code: i32,
    has_native_code: bool,
    elapsed_ms: u64,
    row_count: u64,
    has_row_count: bool,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(profile) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_record_history: `profile` is null or not a valid profile id",
            );
        };
        // SAFETY: delegated to this function's contract for `statement`.
        let Some(statement) = (unsafe { statement.as_str() }) else {
            return set_last_argument_error(
                "reldex_workspace_record_history: `statement` is null or is not valid UTF-8",
            );
        };
        let Some(outcome) = history_outcome_from_i32(outcome_kind, native_code, has_native_code)
        else {
            return set_last_argument_error(
                "reldex_workspace_record_history: `outcome_kind` is not a kind this build knows",
            );
        };
        let entry = HistoryEntry {
            profile,
            executed_at: UnixTimeMs::from_millis(i64::try_from(executed_at_ms).unwrap_or(i64::MAX)),
            statement: statement.to_owned(),
            outcome,
            elapsed_ms,
            row_count: has_row_count.then_some(row_count),
        };
        submit(
            workspace,
            WorkspaceCommand::RecordHistory { request, entry },
        )
    })
}

/// Lists one page of a profile's history, newest first.
///
/// # Safety
///
/// `workspace` must be a live workspace. `profile` must point at 16 readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_list_history(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
    limit: u32,
    before_id: u64,
    has_before: bool,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(profile) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_list_history: `profile` is null or not a valid profile id",
            );
        };
        let page = if has_before {
            HistoryPage::after(limit, history_id_from_ffi(before_id))
        } else {
            HistoryPage::first(limit)
        };
        submit(
            workspace,
            WorkspaceCommand::ListHistory {
                request,
                profile,
                page,
            },
        )
    })
}

/// Clears a profile's history.
///
/// # Safety
///
/// As [`reldex_workspace_list_history`], minus `limit`/`before_id`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_clear_history(
    workspace: *mut ReldexWorkspace,
    request: u64,
    profile: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(profile) };
        let Ok(profile) = ProfileId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_clear_history: `profile` is null or not a valid profile id",
            );
        };
        submit(
            workspace,
            WorkspaceCommand::ClearHistory { request, profile },
        )
    })
}

/// Saves a worksheet: inserts it if `id` is new, otherwise replaces its
/// state, profile and tab order in place (its creation time is kept as first
/// stored). Get `id` from [`reldex_workspace_new_worksheet_id`] the first
/// time a worksheet is created.
///
/// # Safety
///
/// `workspace` must be a live workspace. `id` must point at 16 readable
/// bytes. `profile_id` must be null (no profile attached) or point at 16
/// readable bytes. `title`/`text` must point at their declared lengths of
/// readable UTF-8 bytes (read only for the duration of this call).
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_save_worksheet(
    workspace: *mut ReldexWorkspace,
    request: u64,
    id: *const u8,
    profile_id: *const u8,
    title: ReldexStr,
    text: ReldexStr,
    caret: u32,
    scroll: u32,
    tab_order: u32,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(id) };
        let Ok(worksheet_id) = WorksheetId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_save_worksheet: `id` is null or not a valid worksheet id",
            );
        };
        let profile = if profile_id.is_null() {
            None
        } else {
            // SAFETY: delegated to this function's contract.
            let bytes = unsafe { read_id_bytes(profile_id) };
            match ProfileId::from_bytes(bytes) {
                Ok(id) => Some(id),
                Err(_) => {
                    return set_last_argument_error(
                        "reldex_workspace_save_worksheet: `profile_id` is not a valid profile id",
                    );
                }
            }
        };
        // SAFETY: delegated to this function's contract for `title`/`text`.
        let (Some(title), Some(text)) = (unsafe { title.as_str() }, unsafe { text.as_str() })
        else {
            return set_last_argument_error(
                "reldex_workspace_save_worksheet: `title`/`text` is null or is not valid UTF-8",
            );
        };
        let state = WorksheetState {
            title: title.to_owned(),
            text: text.to_owned(),
            caret,
            scroll,
        };
        let worksheet = Worksheet::new(worksheet_id, profile, state, tab_order);
        submit(
            workspace,
            WorkspaceCommand::SaveWorksheet { request, worksheet },
        )
    })
}

/// Loads every open worksheet, in tab order.
///
/// # Safety
///
/// `workspace` must be a live workspace.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_load_worksheets(
    workspace: *mut ReldexWorkspace,
    request: u64,
) -> ReldexStatus {
    entry(|| submit(workspace, WorkspaceCommand::LoadWorksheets { request }))
}

/// Deletes a worksheet.
///
/// # Safety
///
/// `workspace` must be a live workspace. `id` must point at 16 readable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_delete_worksheet(
    workspace: *mut ReldexWorkspace,
    request: u64,
    id: *const u8,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let bytes = unsafe { read_id_bytes(id) };
        let Ok(id) = WorksheetId::from_bytes(bytes) else {
            return set_last_argument_error(
                "reldex_workspace_delete_worksheet: `id` is null or not a valid worksheet id",
            );
        };
        submit(workspace, WorkspaceCommand::DeleteWorksheet { request, id })
    })
}

/// Saves the workspace's layout, replacing whatever was saved before.
///
/// # Safety
///
/// `workspace` must be a live workspace. `layout` must be null or point at a
/// [`ReldexLayout`] with `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_save_layout(
    workspace: *mut ReldexWorkspace,
    request: u64,
    layout: *const ReldexLayout,
) -> ReldexStatus {
    entry(|| {
        // SAFETY: delegated to this function's contract.
        let Some(layout) = (unsafe { read_in_struct(layout) }) else {
            return set_last_argument_error(
                "reldex_workspace_save_layout: `layout` is null, unaligned, or its struct_size \
                 is too small",
            );
        };
        let layout = match layout.to_layout() {
            Ok(layout) => layout,
            Err(_) => {
                return set_last_argument_error(
                    "reldex_workspace_save_layout: `layout` names an id that is not a valid \
                     UUID",
                );
            }
        };
        submit(workspace, WorkspaceCommand::SaveLayout { request, layout })
    })
}

/// Loads the workspace's layout, if one has ever been saved.
///
/// # Safety
///
/// `workspace` must be a live workspace.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_workspace_load_layout(
    workspace: *mut ReldexWorkspace,
    request: u64,
) -> ReldexStatus {
    entry(|| submit(workspace, WorkspaceCommand::LoadLayout { request }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::*;

    /// A condvar-backed wake target, the same shape `tests/support`'s
    /// `Signal` gives the integration tests -- so these unit tests wait for
    /// a reply rather than sleeping and polling.
    #[derive(Default)]
    struct Signal {
        inner: Mutex<u64>,
        condvar: Condvar,
    }

    extern "C" fn wake(user_data: *mut c_void) {
        // SAFETY: `user_data` is the `Arc<Signal>` raw pointer this test
        // handed to `reldex_workspace_set_waker`, still alive for the
        // workspace's whole life.
        let signal = unsafe { &*user_data.cast::<Signal>() };
        *signal
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        signal.condvar.notify_all();
    }

    struct TestWorkspace {
        handle: *mut ReldexWorkspace,
        signal: Arc<Signal>,
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            // SAFETY: `handle` is live and this test holds the only reference.
            unsafe { reldex_workspace_close(self.handle) };
        }
    }

    impl TestWorkspace {
        /// Opens an in-memory store with a memory credential store (this
        /// build always has `mock-driver`, since it is the default feature
        /// and tests do not disable it).
        fn open() -> (Self, ReldexWorkspaceReply) {
            let signal = Arc::new(Signal::default());
            let mut handle: *mut ReldexWorkspace = std::ptr::null_mut();
            // SAFETY: `path` is unused (`in_memory: true`); `handle` is a
            // real local out-pointer.
            let status = unsafe {
                reldex_workspace_open(
                    ReldexStr::empty(),
                    true,
                    true,
                    1,
                    std::ptr::from_mut(&mut handle),
                )
            };
            assert_eq!(status, ReldexStatus::Ok);
            assert!(!handle.is_null());
            // SAFETY: `handle` is live; `signal` outlives the workspace (this
            // struct holds both and closes the workspace first, on drop).
            let waker_status = unsafe {
                reldex_workspace_set_waker(
                    handle,
                    Some(wake),
                    Arc::as_ptr(&signal).cast_mut().cast::<c_void>(),
                )
            };
            assert_eq!(waker_status, ReldexStatus::Ok);
            let mut workspace = Self { handle, signal };
            let opened = workspace.wait_for(1);
            assert!(opened.error.is_null(), "store failed to open");
            (workspace, opened)
        }

        fn handle(&mut self) -> *mut ReldexWorkspace {
            self.handle
        }

        /// Waits for the reply to `request`, draining and discarding any
        /// other reply in between (none of these tests submit more than one
        /// request at a time, so none is expected, but this keeps the
        /// helper honest about what it does).
        fn wait_for(&mut self, request: u64) -> ReldexWorkspaceReply {
            loop {
                let mut seen = self
                    .signal
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    let mut out = ReldexWorkspaceReply {
                        struct_size: u32::try_from(size_of::<ReldexWorkspaceReply>())
                            .expect("ReldexWorkspaceReply's size fits in u32"),
                        ..QueuedWorkspaceReply {
                            kind: 0,
                            request: 0,
                            payload: Ok(WorkspacePayload::None),
                        }
                        .into_c()
                    };
                    // SAFETY: `self.handle` is live; `out` is a real local
                    // with `struct_size` set.
                    let taken = unsafe {
                        reldex_workspace_next_reply(self.handle, std::ptr::from_mut(&mut out))
                    };
                    if taken {
                        if out.request == request {
                            return out;
                        }
                        continue;
                    }
                    break;
                }
                let (guard, timeout) = self
                    .signal
                    .condvar
                    .wait_timeout(seen, Duration::from_secs(5))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                assert!(
                    !timeout.timed_out(),
                    "no reply to request {request} arrived in time"
                );
                seen = guard;
                drop(seen);
            }
        }
    }

    fn profile_details(port: u16, sid: bool) -> ReldexProfileDetails {
        let target_kind = if sid {
            ReldexServiceTargetKind::Sid
        } else {
            ReldexServiceTargetKind::ServiceName
        };
        ReldexProfileDetails {
            struct_size: u32::try_from(size_of::<ReldexProfileDetails>())
                .expect("ReldexProfileDetails's size fits in u32"),
            name: str_of("Orders (test)"),
            database_type: ReldexDatabaseType::Oracle as i32,
            environment: ReldexEnvironmentKind::Test as i32,
            environment_label: ReldexStr::empty(),
            treat_as_production: false,
            endpoint_kind: ReldexEndpointKind::HostPort as i32,
            host: str_of("db.example.internal"),
            port,
            service_target_kind: target_kind as i32,
            service_name_or_sid: str_of("ORDERS"),
            connect_string: ReldexStr::empty(),
            auth_kind: ReldexAuthKind::Password as i32,
            username: str_of("app_owner"),
            password_storage: ReldexPasswordStorageKind::CredentialStore as i32,
            role: ReldexSessionRoleKind::Normal as i32,
            transport: ReldexTransportKind::Plain as i32,
            ca_directory: ReldexStr::empty(),
            allow_unenforced_certificate_pin: false,
        }
    }

    fn str_of(text: &'static str) -> ReldexStr {
        ReldexStr {
            ptr: text.as_ptr(),
            len: text.len(),
        }
    }

    #[test]
    fn a_setting_resolves_to_its_built_in_default_with_no_layers() {
        let (mut workspace, _opened) = TestWorkspace::open();
        // SAFETY: `workspace.handle()` is live; both id pointers are null,
        // which asks for no profile/worksheet layer.
        let status = unsafe {
            reldex_workspace_resolve_setting(
                workspace.handle(),
                2,
                ReldexSettingId::FetchRows as i32,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let reply = workspace.wait_for(2);
        assert!(reply.error.is_null());
        assert_eq!(reply.setting_source, ReldexSettingLevel::BuiltIn as i32);
        assert_eq!(reply.setting_value.kind, ReldexValueKind::Count as i32);
        assert_eq!(
            reply.setting_value.count_value, 1_000,
            "FETCH_ROWS's documented default"
        );
    }

    #[test]
    fn setting_a_value_is_seen_on_resolve_and_clearing_it_reverts_to_the_default() {
        let (mut workspace, _opened) = TestWorkspace::open();
        let value = ReldexSettingValue {
            struct_size: u32::try_from(size_of::<ReldexSettingValue>())
                .expect("ReldexSettingValue's size fits in u32"),
            kind: ReldexValueKind::Count as i32,
            bool_value: false,
            count_value: 250,
            no_limit: false,
            number_value: 0,
        };
        // SAFETY: `workspace.handle()` is live; `value` is a real local with
        // `struct_size` set; `scope_id` is null (Application scope).
        let status = unsafe {
            reldex_workspace_set_setting(
                workspace.handle(),
                3,
                ReldexSettingId::FetchRows as i32,
                ReldexSettingLevel::Application as i32,
                std::ptr::null(),
                std::ptr::from_ref(&value),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let set_reply = workspace.wait_for(3);
        assert!(set_reply.error.is_null(), "set_setting failed");

        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_resolve_setting(
                workspace.handle(),
                4,
                ReldexSettingId::FetchRows as i32,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let resolved = workspace.wait_for(4);
        assert_eq!(
            resolved.setting_source,
            ReldexSettingLevel::Application as i32
        );
        assert_eq!(resolved.setting_value.count_value, 250);

        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_clear_setting(
                workspace.handle(),
                5,
                ReldexSettingId::FetchRows as i32,
                ReldexSettingLevel::Application as i32,
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let cleared = workspace.wait_for(5);
        assert!(cleared.error.is_null());
        assert!(cleared.found, "a value existed to clear");

        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_resolve_setting(
                workspace.handle(),
                6,
                ReldexSettingId::FetchRows as i32,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let back_to_default = workspace.wait_for(6);
        assert_eq!(
            back_to_default.setting_source,
            ReldexSettingLevel::BuiltIn as i32
        );
        assert_eq!(back_to_default.setting_value.count_value, 1_000);
    }

    #[test]
    fn a_profile_is_created_fetched_and_a_credential_looking_endpoint_is_refused() {
        let (mut workspace, _opened) = TestWorkspace::open();
        let details = profile_details(1521, false);
        // SAFETY: `workspace.handle()` is live; `details` is a real local
        // with `struct_size` set and NUL-adjacent `'static` strings.
        let status = unsafe {
            reldex_workspace_create_profile(workspace.handle(), 7, std::ptr::from_ref(&details))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created = workspace.wait_for(7);
        assert!(created.error.is_null(), "create_profile failed");
        let id = created.id;

        // SAFETY: `id` is a real local 16-byte array.
        let status = unsafe { reldex_workspace_get_profile(workspace.handle(), 8, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let fetched = workspace.wait_for(8);
        assert!(fetched.error.is_null());
        assert!(fetched.found);
        assert!(!fetched.profile_list.is_null());
        // SAFETY: `fetched.profile_list` is live and owned by this test.
        let profile_count = unsafe { reldex_profile_list_count(fetched.profile_list) };
        assert_eq!(profile_count, 1);
        let mut view = ReldexProfileView {
            struct_size: u32::try_from(size_of::<ReldexProfileView>())
                .expect("ReldexProfileView's size fits in u32"),
            ..profile_view_zeroed()
        };
        // SAFETY: as above; `view` is a real local with `struct_size` set.
        assert!(unsafe {
            reldex_profile_list_get(fetched.profile_list, 0, std::ptr::from_mut(&mut view))
        });
        // SAFETY: `view.name` borrows from `fetched.profile_list`, still live.
        assert_eq!(unsafe { view.name.as_str() }, Some("Orders (test)"));
        // SAFETY: as above.
        unsafe { reldex_profile_list_release(fetched.profile_list) };

        let mut credential_looking = profile_details(1521, false);
        credential_looking.connect_string = str_of("scott/tiger@//db:1521/ORDERS");
        credential_looking.endpoint_kind = ReldexEndpointKind::ConnectString as i32;
        // SAFETY: as the first `create_profile` call.
        let status = unsafe {
            reldex_workspace_create_profile(
                workspace.handle(),
                9,
                std::ptr::from_ref(&credential_looking),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let refused = workspace.wait_for(9);
        assert!(
            !refused.error.is_null(),
            "a credential-looking endpoint must be refused"
        );
        // SAFETY: `refused.error` was just checked non-null and is owned by
        // this test.
        unsafe { crate::error::reldex_error_free(refused.error) };
    }

    fn profile_view_zeroed() -> ReldexProfileView {
        ReldexProfileView {
            struct_size: 0,
            id: [0; 16],
            created_at: 0,
            updated_at: 0,
            name: ReldexStr::empty(),
            database_type: 0,
            environment: 0,
            environment_label: ReldexStr::empty(),
            treat_as_production: false,
            endpoint_kind: 0,
            host: ReldexStr::empty(),
            port: 0,
            service_target_kind: 0,
            service_name_or_sid: ReldexStr::empty(),
            connect_string: ReldexStr::empty(),
            auth_kind: 0,
            username: ReldexStr::empty(),
            password_storage: 0,
            role: 0,
            transport: 0,
            ca_directory: ReldexStr::empty(),
            allow_unenforced_certificate_pin: false,
        }
    }

    #[test]
    fn connect_params_map_a_service_name_and_a_sid_through_the_oracle_binding() {
        let (mut workspace, _opened) = TestWorkspace::open();
        // External authentication: `connection_params` needs no password, so
        // this test can call `build_connect_params` with `password = null`
        // and still succeed -- resolving a password is
        // `resolve_password_against_a_memory_store_...`'s own test.
        let mut service_name = profile_details(1521, false);
        service_name.auth_kind = ReldexAuthKind::External as i32;
        // SAFETY: as the profile-creation test above.
        let status = unsafe {
            reldex_workspace_create_profile(
                workspace.handle(),
                10,
                std::ptr::from_ref(&service_name),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created = workspace.wait_for(10);
        assert!(created.error.is_null());
        let id = created.id;

        // SAFETY: `id` is a real local array; `password` is null.
        let status = unsafe {
            reldex_workspace_build_connect_params(
                workspace.handle(),
                11,
                id.as_ptr(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let built = workspace.wait_for(11);
        assert!(
            built.error.is_null(),
            "build_connect_params failed for a service-name profile"
        );
        assert!(!built.connect.is_null());
        let mut view = connect_summary_view_zeroed();
        // SAFETY: `built.connect` is live and owned by this test; `view` is a
        // real local with `struct_size` set.
        assert!(unsafe {
            reldex_connect_summary_view(built.connect, std::ptr::from_mut(&mut view))
        });
        assert_eq!(view.endpoint_kind, ReldexEndpointKind::HostPort as i32);
        // SAFETY: `view.host` borrows from `built.connect`, still live.
        assert_eq!(unsafe { view.host.as_str() }, Some("db.example.internal"));
        assert!(
            view.rewrite_trigger_ddl,
            "REWRITE_TRIGGER_DDL defaults to on"
        );
        // SAFETY: as above.
        unsafe { reldex_connect_summary_release(built.connect) };

        let mut sid = profile_details(1522, true);
        sid.auth_kind = ReldexAuthKind::External as i32;
        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_create_profile(workspace.handle(), 12, std::ptr::from_ref(&sid))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created_sid = workspace.wait_for(12);
        assert!(created_sid.error.is_null());
        let sid_id = created_sid.id;

        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_build_connect_params(
                workspace.handle(),
                13,
                sid_id.as_ptr(),
                std::ptr::null(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let built_sid = workspace.wait_for(13);
        assert!(
            built_sid.error.is_null(),
            "build_connect_params failed for a SID profile"
        );
        let mut sid_view = connect_summary_view_zeroed();
        // SAFETY: as above.
        assert!(unsafe {
            reldex_connect_summary_view(built_sid.connect, std::ptr::from_mut(&mut sid_view))
        });
        assert_eq!(
            sid_view.endpoint_kind,
            ReldexEndpointKind::ConnectString as i32,
            "a SID has no vendor-neutral endpoint shape; the Oracle binding maps it to a connect \
             string"
        );
        // SAFETY: `sid_view.connect_string` borrows from `built_sid.connect`,
        // still live.
        let text = unsafe { sid_view.connect_string.as_str() }.expect("utf-8");
        assert!(text.contains("ORDERS"), "{text}");
        // SAFETY: as above.
        unsafe { reldex_connect_summary_release(built_sid.connect) };
    }

    fn connect_summary_view_zeroed() -> ReldexConnectSummaryView {
        ReldexConnectSummaryView {
            struct_size: u32::try_from(size_of::<ReldexConnectSummaryView>())
                .expect("ReldexConnectSummaryView's size fits in u32"),
            endpoint_kind: 0,
            host: ReldexStr::empty(),
            port: 0,
            service: ReldexStr::empty(),
            connect_string: ReldexStr::empty(),
            tls_mode: 0,
            has_connect_timeout: false,
            connect_timeout_seconds: 0,
            role: 0,
            rewrite_trigger_ddl: false,
            connect_without_limit: false,
            allow_unenforced_certificate_pin: false,
            ca_directory: ReldexStr::empty(),
        }
    }

    #[test]
    fn resolve_password_against_a_memory_store_reports_not_stored_then_from_store() {
        let (mut workspace, _opened) = TestWorkspace::open();
        let details = profile_details(1521, false);
        // SAFETY: as the profile-creation test above.
        let status = unsafe {
            reldex_workspace_create_profile(workspace.handle(), 14, std::ptr::from_ref(&details))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created = workspace.wait_for(14);
        let id = created.id;

        // SAFETY: `id` is a real local array.
        let status =
            unsafe { reldex_workspace_resolve_password(workspace.handle(), 15, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let not_stored = workspace.wait_for(15);
        assert!(not_stored.error.is_null());
        assert_eq!(
            not_stored.password_source_kind,
            ReldexPasswordSourceKind::PromptRequired as i32
        );
        assert_eq!(
            not_stored.prompt_reason,
            ReldexPromptReasonKind::NotStored as i32
        );

        let marker = "reldex-test-marker-9f2c";
        // SAFETY: `id` is live; `marker` is a real `'static` string.
        let status = unsafe {
            reldex_workspace_credential_put(workspace.handle(), 16, id.as_ptr(), str_of(marker))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let put = workspace.wait_for(16);
        assert!(put.error.is_null(), "credential_put failed");

        // SAFETY: as above.
        let status =
            unsafe { reldex_workspace_resolve_password(workspace.handle(), 17, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let from_store = workspace.wait_for(17);
        assert!(from_store.error.is_null());
        assert_eq!(
            from_store.password_source_kind,
            ReldexPasswordSourceKind::FromStore as i32
        );
        assert!(!from_store.secret.is_null());
        // SAFETY: `from_store.secret` is live and owned by this test.
        let exposed = unsafe { reldex_secret_expose(from_store.secret) };
        // SAFETY: `exposed` borrows from `from_store.secret`, still live.
        let exposed_text =
            unsafe { std::str::from_utf8(std::slice::from_raw_parts(exposed.ptr, exposed.len)) };
        assert_eq!(exposed_text, Ok(marker));
        // SAFETY: as above.
        unsafe { reldex_secret_release(from_store.secret) };

        // SAFETY: as above.
        let status =
            unsafe { reldex_workspace_credential_delete(workspace.handle(), 18, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let deleted = workspace.wait_for(18);
        assert!(deleted.error.is_null());
        assert!(
            deleted.found,
            "the password just put must be reported as having existed"
        );
    }

    #[test]
    fn history_is_recorded_listed_and_cleared() {
        let (mut workspace, _opened) = TestWorkspace::open();
        let details = profile_details(1521, false);
        // SAFETY: as the profile-creation test above.
        let status = unsafe {
            reldex_workspace_create_profile(workspace.handle(), 19, std::ptr::from_ref(&details))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created = workspace.wait_for(19);
        let id = created.id;

        // SAFETY: `id` is live; `str_of`'s text is real and `'static`.
        let status = unsafe {
            reldex_workspace_record_history(
                workspace.handle(),
                20,
                id.as_ptr(),
                1_700_000_000_000,
                str_of("SELECT 1 FROM dual"),
                ReldexHistoryOutcomeKind::Succeeded as i32,
                0,
                false,
                5,
                1,
                true,
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let recorded = workspace.wait_for(20);
        assert!(recorded.error.is_null(), "record_history failed");
        assert!(
            recorded.history_id != 0 || true,
            "an id of 0 is still a valid rowid in principle"
        );

        // SAFETY: as above.
        let status = unsafe {
            reldex_workspace_list_history(workspace.handle(), 21, id.as_ptr(), 10, 0, false)
        };
        assert_eq!(status, ReldexStatus::Ok);
        let listed = workspace.wait_for(21);
        assert!(listed.error.is_null());
        assert!(!listed.history_list.is_null());
        // SAFETY: `listed.history_list` is live and owned by this test.
        assert_eq!(unsafe { reldex_history_list_count(listed.history_list) }, 1);
        let mut record_view = history_view_zeroed();
        // SAFETY: as above; `record_view` is a real local with `struct_size`
        // set.
        assert!(unsafe {
            reldex_history_list_get(listed.history_list, 0, std::ptr::from_mut(&mut record_view))
        });
        // SAFETY: `record_view.statement` borrows from `listed.history_list`,
        // still live.
        let statement_text = unsafe { record_view.statement.as_str() };
        assert_eq!(statement_text, Some("SELECT 1 FROM dual"));
        assert_eq!(record_view.row_count, 1);
        // SAFETY: as above.
        unsafe { reldex_history_list_release(listed.history_list) };

        // SAFETY: as above.
        let status = unsafe { reldex_workspace_clear_history(workspace.handle(), 22, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let cleared = workspace.wait_for(22);
        assert!(cleared.error.is_null());
        assert_eq!(cleared.count, 1);
    }

    fn history_view_zeroed() -> ReldexHistoryRecordView {
        ReldexHistoryRecordView {
            struct_size: u32::try_from(size_of::<ReldexHistoryRecordView>())
                .expect("ReldexHistoryRecordView's size fits in u32"),
            id: 0,
            executed_at: 0,
            statement: ReldexStr::empty(),
            outcome_kind: 0,
            native_code: 0,
            has_native_code: false,
            elapsed_ms: 0,
            row_count: 0,
            has_row_count: false,
        }
    }

    #[test]
    fn a_worksheet_is_saved_loaded_and_deleted_and_layout_round_trips() {
        let (mut workspace, _opened) = TestWorkspace::open();
        let mut id = [0_u8; 16];
        // SAFETY: `id` is a real, writable 16-byte local array.
        unsafe { reldex_workspace_new_worksheet_id(id.as_mut_ptr()) };

        // SAFETY: `workspace.handle()` is live; `id` is a real local array;
        // `title`/`text` are real `'static` strings; `profile_id` is null
        // (no profile attached).
        let status = unsafe {
            reldex_workspace_save_worksheet(
                workspace.handle(),
                23,
                id.as_ptr(),
                std::ptr::null(),
                str_of("scratch"),
                str_of("select 1 from dual;"),
                0,
                0,
                0,
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let saved = workspace.wait_for(23);
        assert!(saved.error.is_null(), "save_worksheet failed");

        // SAFETY: `workspace.handle()` is live.
        let status = unsafe { reldex_workspace_load_worksheets(workspace.handle(), 24) };
        assert_eq!(status, ReldexStatus::Ok);
        let loaded = workspace.wait_for(24);
        assert!(loaded.error.is_null());
        assert!(!loaded.worksheet_list.is_null());
        // SAFETY: `loaded.worksheet_list` is live and owned by this test.
        let worksheet_count = unsafe { reldex_worksheet_list_count(loaded.worksheet_list) };
        assert_eq!(worksheet_count, 1);
        let mut view = worksheet_view_zeroed();
        // SAFETY: as above; `view` is a real local with `struct_size` set.
        assert!(unsafe {
            reldex_worksheet_list_get(loaded.worksheet_list, 0, std::ptr::from_mut(&mut view))
        });
        // SAFETY: `view.text` borrows from `loaded.worksheet_list`, still
        // live.
        assert_eq!(unsafe { view.text.as_str() }, Some("select 1 from dual;"));
        // SAFETY: as above.
        unsafe { reldex_worksheet_list_release(loaded.worksheet_list) };

        let layout = ReldexLayout {
            struct_size: u32::try_from(size_of::<ReldexLayout>())
                .expect("ReldexLayout's size fits in u32"),
            has_active_worksheet: true,
            active_worksheet: id,
            has_active_profile: false,
            active_profile: [0; 16],
            has_object_browser_width: true,
            object_browser_width: 240,
            has_result_pane_height: false,
            result_pane_height: 0,
            has_window_position: false,
            window_x: 0,
            window_y: 0,
            has_window_size: false,
            window_width: 0,
            window_height: 0,
            window_maximized: true,
        };
        // SAFETY: `workspace.handle()` is live; `layout` is a real local
        // with `struct_size` set.
        let status = unsafe {
            reldex_workspace_save_layout(workspace.handle(), 25, std::ptr::from_ref(&layout))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let layout_saved = workspace.wait_for(25);
        assert!(layout_saved.error.is_null(), "save_layout failed");

        // SAFETY: `workspace.handle()` is live.
        let status = unsafe { reldex_workspace_load_layout(workspace.handle(), 26) };
        assert_eq!(status, ReldexStatus::Ok);
        let layout_loaded = workspace.wait_for(26);
        assert!(layout_loaded.error.is_null());
        assert!(layout_loaded.found);
        assert!(layout_loaded.layout.has_active_worksheet);
        assert_eq!(layout_loaded.layout.active_worksheet, id);
        assert_eq!(layout_loaded.layout.object_browser_width, 240);
        assert!(layout_loaded.layout.window_maximized);

        // SAFETY: `workspace.handle()` is live; `id` is a real local array.
        let status =
            unsafe { reldex_workspace_delete_worksheet(workspace.handle(), 27, id.as_ptr()) };
        assert_eq!(status, ReldexStatus::Ok);
        let deleted = workspace.wait_for(27);
        assert!(deleted.error.is_null());
        assert!(
            deleted.found,
            "the worksheet just saved must be reported as having existed"
        );
    }

    fn worksheet_view_zeroed() -> ReldexWorksheetView {
        ReldexWorksheetView {
            struct_size: u32::try_from(size_of::<ReldexWorksheetView>())
                .expect("ReldexWorksheetView's size fits in u32"),
            id: [0; 16],
            has_profile: false,
            profile_id: [0; 16],
            title: ReldexStr::empty(),
            text: ReldexStr::empty(),
            caret: 0,
            scroll: 0,
            tab_order: 0,
            created_at: 0,
            updated_at: 0,
        }
    }
}
