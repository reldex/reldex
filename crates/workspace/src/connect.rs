//! Turning a profile and its resolved settings into what `db-core` consumes:
//! [`ConnectionParams`] for opening a session, and the options a
//! [`Statement`] and a session's server output take.
//!
//! Every function here is pure — no I/O, no clock, no thread — so the mapping
//! can be checked exhaustively without a database.
//!
//! # The driver binding
//!
//! Most of a profile maps onto the vendor-neutral fields of
//! [`ConnectionParams`] directly. A small residue cannot: a SID has no
//! vendor-neutral endpoint shape, and "no connect limit", the trigger rewrite
//! switch, the CA directory and the certificate-pin opt-out are driver
//! extensions (ADR-0002 D7: vendor-specific parameters travel in
//! [`Extensions`], keyed by the driver's own constants). That residue goes
//! through a [`DriverBinding`], implemented once per database type by the
//! composition root that links the driver (M2.11) — the only place allowed
//! to name a concrete driver (`ARCHITECTURE.md` §2). This crate names no
//! extension key and builds no vendor syntax. ADR-0006 "Driver binding".

use std::num::NonZeroUsize;
use std::path::Path;

use reldex_db_driver_api::{
    ConnectionParams, Credentials, DbError, Endpoint, Extensions, Secret, ServerOutputSetting,
    Statement, TlsMode,
};

use crate::profile::{
    Authentication, DatabaseType, Profile, ProfileEndpoint, ProfileError, ServiceTarget, Transport,
};
use crate::settings::{
    ByteLimit, CONNECT_TIMEOUT, FETCH_ROWS, FETCHES_IN_FLIGHT, REWRITE_TRIGGER_DDL, ResolveContext,
    Resolved, SERVER_OUTPUT_BUFFER, SERVER_OUTPUT_ENABLED, STATEMENT_TIME_LIMIT, TimeLimit,
};

/// The profile options only a driver knows how to express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DriverOptions<'a> {
    /// The user chose "no limit" for the connect timeout. The vendor-neutral
    /// `connect_timeout` cannot say so — its absence means "the driver's
    /// default" — so the binding must.
    pub connect_without_limit: bool,
    /// Whether trigger DDL is rewritten so it can run (owner decision
    /// 2026-09-19; on by default).
    pub rewrite_trigger_ddl: bool,
    /// The directory of certificate authorities to trust, if any.
    pub ca_directory: Option<&'a Path>,
    /// The C-6 guard's opt-out: open a session whose descriptor asks for a
    /// certificate pin the driver cannot enforce.
    pub allow_unenforced_certificate_pin: bool,
}

/// How one database type's driver spells what the vendor-neutral
/// [`ConnectionParams`] cannot. Implemented where the driver is linked.
///
/// Errors are [`DbError`]s of kind `Configuration`, the same shape the driver
/// itself uses for a parameter it refuses.
pub trait DriverBinding {
    /// The database type this binding serves. [`connection_params`] refuses a
    /// profile of any other type.
    fn database_type(&self) -> DatabaseType;

    /// The endpoint for a host, port and SID.
    ///
    /// A binding calls the builder its driver exports for this (the first
    /// driver's is `sid_endpoint` in `reldex-driver-oracle-thin`, wired in
    /// M2.11) rather than spelling vendor syntax itself.
    ///
    /// # Errors
    ///
    /// A `Configuration` [`DbError`] when the driver cannot address a SID, or
    /// when a part is not a plain name.
    fn sid_endpoint(
        &self,
        host: &str,
        port: u16,
        sid: &str,
        transport: Transport,
    ) -> Result<Endpoint, DbError>;

    /// The driver's extension bag for these options.
    ///
    /// # Errors
    ///
    /// A `Configuration` [`DbError`] when an option cannot be expressed.
    fn extensions(&self, options: &DriverOptions<'_>) -> Result<Extensions, DbError>;
}

/// Why a profile could not be turned into [`ConnectionParams`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectError {
    /// The profile's details do not validate.
    InvalidProfile(ProfileError),
    /// The binding serves another database type than the profile's.
    WrongBinding {
        /// The profile's type.
        profile: DatabaseType,
        /// The binding's type.
        binding: DatabaseType,
    },
    /// The profile authenticates with a password and none was supplied: the
    /// caller asks the user (the credential store did not hold one, or the
    /// profile is [`crate::PasswordStorage::PromptEachTime`]).
    PasswordRequired,
    /// A password was supplied for a profile that does not use one. Refused
    /// rather than dropped, because a caller that fetched one has a bug worth
    /// seeing.
    PasswordNotUsed,
    /// The driver binding refused an option.
    Binding(DbError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(error) => write!(f, "invalid profile: {error}"),
            Self::WrongBinding { profile, binding } => write!(
                f,
                "a {binding:?} driver binding cannot open a {profile:?} profile"
            ),
            Self::PasswordRequired => f.write_str("this profile needs a password"),
            Self::PasswordNotUsed => {
                f.write_str("a password was supplied for a profile that does not use one")
            }
            Self::Binding(error) => write!(f, "the driver refused the profile: {error}"),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidProfile(error) => Some(error),
            Self::Binding(error) => Some(error),
            _ => None,
        }
    }
}

/// The settings fixed when a connection opens, with their provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectSettings {
    /// [`CONNECT_TIMEOUT`].
    pub connect_timeout: Resolved<TimeLimit>,
    /// [`REWRITE_TRIGGER_DDL`].
    pub rewrite_trigger_ddl: Resolved<bool>,
}

impl ConnectSettings {
    /// Resolves them. A worksheet layer in `context` is ignored for these:
    /// neither may be set at the worksheet level.
    #[must_use]
    pub fn resolve(context: &ResolveContext<'_>) -> Self {
        Self {
            connect_timeout: context.resolve(CONNECT_TIMEOUT),
            rewrite_trigger_ddl: context.resolve(REWRITE_TRIGGER_DDL),
        }
    }
}

/// Builds the parameters for opening a session on `profile`.
///
/// `password` comes from the credential store (M2.10) or a prompt, and moves
/// straight into the result: it never passes through this crate's store, and
/// [`ConnectionParams`]'s `Debug` redacts it.
///
/// # Errors
///
/// [`ConnectError`]: an invalid profile, a binding for another database type,
/// a password missing or superfluous, or a binding refusal.
pub fn connection_params(
    profile: &Profile,
    settings: &ConnectSettings,
    password: Option<Secret>,
    binding: &dyn DriverBinding,
) -> Result<ConnectionParams, ConnectError> {
    let details = profile.details();
    details.validate().map_err(ConnectError::InvalidProfile)?;
    if binding.database_type() != details.database {
        return Err(ConnectError::WrongBinding {
            profile: details.database,
            binding: binding.database_type(),
        });
    }

    let credentials = match (&details.authentication, password) {
        (Authentication::Password { username, .. }, Some(password)) => Credentials::UserPassword {
            username: username.clone(),
            password,
        },
        (Authentication::Password { .. }, None) => return Err(ConnectError::PasswordRequired),
        (Authentication::External, None) => Credentials::External,
        (Authentication::External, Some(_)) => return Err(ConnectError::PasswordNotUsed),
    };

    let transport = details.tls.transport;
    let endpoint = match &details.endpoint {
        ProfileEndpoint::HostPort {
            host,
            port,
            target: ServiceTarget::ServiceName(service),
        } => Endpoint::HostPort {
            host: host.clone(),
            port: *port,
            service: service.clone(),
        },
        ProfileEndpoint::HostPort {
            host,
            port,
            target: ServiceTarget::Sid(sid),
        } => binding
            .sid_endpoint(host, *port, sid, transport)
            .map_err(ConnectError::Binding)?,
        ProfileEndpoint::ConnectString(text) => Endpoint::ConnectString(text.clone()),
    };

    let options = DriverOptions {
        connect_without_limit: settings.connect_timeout.value == TimeLimit::NoLimit,
        rewrite_trigger_ddl: settings.rewrite_trigger_ddl.value,
        ca_directory: details.tls.ca_directory.as_deref(),
        allow_unenforced_certificate_pin: details.tls.allow_unenforced_certificate_pin,
    };
    let extensions = binding
        .extensions(&options)
        .map_err(ConnectError::Binding)?;

    let mut params = ConnectionParams::new(endpoint, credentials)
        .with_role(details.role)
        .with_tls(match transport {
            Transport::Plain => TlsMode::Disabled,
            Transport::Tls => TlsMode::Required,
        })
        .with_extensions(extensions);
    if let Some(timeout) = settings.connect_timeout.value.as_duration() {
        params = params.with_connect_timeout(timeout);
    }
    Ok(params)
}

/// The settings read for each statement a worksheet runs, with provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementSettings {
    /// [`STATEMENT_TIME_LIMIT`]: armed before the statement starts. Never
    /// Cancel (`SPEC.md` §10).
    pub time_limit: Resolved<TimeLimit>,
    /// [`FETCH_ROWS`].
    pub fetch_rows: Resolved<u32>,
    /// [`FETCHES_IN_FLIGHT`] — for the result view's pipeline, not the
    /// statement.
    pub fetches_in_flight: Resolved<u32>,
}

impl StatementSettings {
    /// Resolves them.
    #[must_use]
    pub fn resolve(context: &ResolveContext<'_>) -> Self {
        Self {
            time_limit: context.resolve(STATEMENT_TIME_LIMIT),
            fetch_rows: context.resolve(FETCH_ROWS),
            fetches_in_flight: context.resolve(FETCHES_IN_FLIGHT),
        }
    }

    /// Arms the time limit and the fetch-size hint on a statement.
    ///
    /// For a freshly built statement: with "no limit" it arms nothing, and
    /// [`Statement`] has no way to disarm a deadline a caller set earlier.
    ///
    /// **A trap for M5.2 Stage B.** This unconditionally sends
    /// `results.fetch_rows` (default 1,000) as the wire array size via
    /// [`Statement::with_fetch_rows`]. Nothing on the product path calls
    /// `apply` today; M5.6 measured that a 1,000-row array costs seconds per
    /// fetch for wide rows on Oracle 19c
    /// (`docs/exec-plans/active/phase-1-fetch-benchmark.md`, "The wire
    /// array"). Stage B must not start calling this as it stands — the wire
    /// array needs to be bounded by `results.round_trip_bytes` and the
    /// row's width (ADR-0004 RS2), not by the row-count setting alone.
    #[must_use]
    pub fn apply(&self, statement: Statement) -> Statement {
        let mut statement = statement;
        if let Some(deadline) = self.time_limit.value.as_duration() {
            statement = statement.with_deadline(deadline);
        }
        if let Some(rows) = usize::try_from(self.fetch_rows.value)
            .ok()
            .and_then(NonZeroUsize::new)
        {
            statement = statement.with_fetch_rows(rows);
        }
        statement
    }
}

/// The server output settings for a worksheet, with provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerOutputSettings {
    /// [`SERVER_OUTPUT_ENABLED`].
    pub enabled: Resolved<bool>,
    /// [`SERVER_OUTPUT_BUFFER`].
    pub buffer: Resolved<ByteLimit>,
}

impl ServerOutputSettings {
    /// Resolves them.
    #[must_use]
    pub fn resolve(context: &ResolveContext<'_>) -> Self {
        Self {
            enabled: context.resolve(SERVER_OUTPUT_ENABLED),
            buffer: context.resolve(SERVER_OUTPUT_BUFFER),
        }
    }

    /// What to ask the session for (`DatabaseSession::set_server_output`,
    /// M2.7). The session answers with the setting actually in force, which
    /// is what a UI must show (ADR-0002 T1).
    #[must_use]
    pub fn setting(&self) -> ServerOutputSetting {
        if self.enabled.value {
            ServerOutputSetting::Enabled(self.buffer.value.into())
        } else {
            ServerOutputSetting::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::time::Duration;

    use reldex_db_driver_api::{ErrorKind, ExtensionValue, ServerOutputBuffer, SessionRole};

    use super::*;
    use crate::profile::{Environment, PasswordStorage, ProfileDetails, TlsOptions};
    use crate::settings::{ApplicationScope, Level, ProfileScope, SettingsLayer, WorksheetScope};

    /// Records what it was asked, in keys this test owns — so the test proves
    /// the options reached the binding, not what any real driver calls them.
    struct Recorder;

    impl DriverBinding for Recorder {
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
            Ok(Endpoint::ConnectString(format!(
                "fake://{host}:{port}/sid/{sid}?transport={transport:?}"
            )))
        }

        fn extensions(&self, options: &DriverOptions<'_>) -> Result<Extensions, DbError> {
            let mut extensions = Extensions::new();
            extensions
                .set(
                    "test.no_limit",
                    ExtensionValue::Flag(options.connect_without_limit),
                )
                .set(
                    "test.rewrite",
                    ExtensionValue::Flag(options.rewrite_trigger_ddl),
                )
                .set(
                    "test.pin",
                    ExtensionValue::Flag(options.allow_unenforced_certificate_pin),
                );
            if let Some(directory) = options.ca_directory {
                extensions.set(
                    "test.ca",
                    ExtensionValue::Text(directory.display().to_string()),
                );
            }
            Ok(extensions)
        }
    }

    fn details(endpoint: ProfileEndpoint) -> ProfileDetails {
        ProfileDetails {
            name: "p".to_owned(),
            database: DatabaseType::Oracle,
            environment: Environment::Test,
            treat_as_production: false,
            endpoint,
            authentication: Authentication::Password {
                username: "scott".to_owned(),
                storage: PasswordStorage::CredentialStore,
            },
            role: SessionRole::Normal,
            tls: TlsOptions::default(),
        }
    }

    fn host_port(target: ServiceTarget) -> ProfileEndpoint {
        ProfileEndpoint::HostPort {
            host: "db.example.internal".to_owned(),
            port: 1522,
            target,
        }
    }

    fn defaults() -> ConnectSettings {
        ConnectSettings::resolve(&ResolveContext::new())
    }

    fn flag(params: &ConnectionParams, key: &str) -> Option<bool> {
        match params.extensions().get(key) {
            Some(ExtensionValue::Flag(value)) => Some(*value),
            _ => None,
        }
    }

    fn params_for(details: ProfileDetails, settings: &ConnectSettings) -> ConnectionParams {
        let profile = Profile::create(details).expect("valid");
        connection_params(&profile, settings, Some(Secret::new("pw")), &Recorder).expect("maps")
    }

    #[test]
    fn a_service_name_becomes_the_neutral_host_port_endpoint() {
        let params = params_for(
            details(host_port(ServiceTarget::ServiceName("ORDERS".to_owned()))),
            &defaults(),
        );
        match params.endpoint() {
            Endpoint::HostPort {
                host,
                port,
                service,
            } => {
                assert_eq!(host, "db.example.internal");
                assert_eq!(*port, 1522);
                assert_eq!(service, "ORDERS");
            }
            other => panic!("expected host/port, got {other:?}"),
        }
        assert_eq!(params.tls(), TlsMode::Disabled);
        assert_eq!(params.role(), SessionRole::Normal);
    }

    #[test]
    fn a_sid_goes_through_the_binding_with_the_transport() {
        let mut details = details(host_port(ServiceTarget::Sid("ORCL".to_owned())));
        details.tls.transport = Transport::Tls;
        let params = params_for(details, &defaults());
        match params.endpoint() {
            Endpoint::ConnectString(text) => {
                assert_eq!(
                    text,
                    "fake://db.example.internal:1522/sid/ORCL?transport=Tls"
                );
            }
            other => panic!("expected the binding's endpoint, got {other:?}"),
        }
        assert_eq!(params.tls(), TlsMode::Required);
    }

    #[test]
    fn a_connect_string_is_passed_verbatim() {
        let descriptor = "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=h)(PORT=2484))\
                          (CONNECT_DATA=(SERVICE_NAME=s)))";
        let params = params_for(
            details(ProfileEndpoint::ConnectString(descriptor.to_owned())),
            &defaults(),
        );
        assert!(matches!(
            params.endpoint(),
            Endpoint::ConnectString(text) if text == descriptor
        ));
    }

    #[test]
    fn the_default_connect_settings_are_a_15_second_limit_and_the_rewrite_on() {
        let params = params_for(
            details(host_port(ServiceTarget::ServiceName("S".to_owned()))),
            &defaults(),
        );
        assert_eq!(params.connect_timeout(), Some(Duration::from_secs(15)));
        assert_eq!(flag(&params, "test.no_limit"), Some(false));
        assert_eq!(flag(&params, "test.rewrite"), Some(true));
        assert_eq!(flag(&params, "test.pin"), Some(false));
        assert!(params.extensions().get("test.ca").is_none());
    }

    #[test]
    fn no_connect_limit_sets_no_neutral_timeout_and_tells_the_binding() {
        let mut profile_layer = SettingsLayer::<ProfileScope>::new();
        profile_layer
            .set(CONNECT_TIMEOUT, TimeLimit::NoLimit)
            .expect("allowed");
        profile_layer
            .set(REWRITE_TRIGGER_DDL, false)
            .expect("allowed");
        let settings =
            ConnectSettings::resolve(&ResolveContext::new().with_profile(&profile_layer));
        assert_eq!(settings.connect_timeout.source, Level::Profile);
        let params = params_for(
            details(host_port(ServiceTarget::ServiceName("S".to_owned()))),
            &settings,
        );
        assert_eq!(params.connect_timeout(), None);
        assert_eq!(flag(&params, "test.no_limit"), Some(true));
        assert_eq!(flag(&params, "test.rewrite"), Some(false));
    }

    #[test]
    fn tls_options_reach_the_binding() {
        let mut details = details(host_port(ServiceTarget::ServiceName("S".to_owned())));
        details.tls = TlsOptions {
            transport: Transport::Tls,
            ca_directory: Some(PathBuf::from("/etc/reldex/ca")),
            allow_unenforced_certificate_pin: true,
        };
        let params = params_for(details, &defaults());
        assert_eq!(params.tls(), TlsMode::Required);
        assert_eq!(flag(&params, "test.pin"), Some(true));
        assert!(matches!(
            params.extensions().get("test.ca"),
            Some(ExtensionValue::Text(text)) if text.contains("ca")
        ));
    }

    #[test]
    fn password_rules_are_explicit() {
        let password = Profile::create(details(host_port(ServiceTarget::ServiceName(
            "S".to_owned(),
        ))))
        .expect("valid");
        assert!(matches!(
            connection_params(&password, &defaults(), None, &Recorder),
            Err(ConnectError::PasswordRequired)
        ));

        let mut external = details(host_port(ServiceTarget::ServiceName("S".to_owned())));
        external.authentication = Authentication::External;
        let external = Profile::create(external).expect("valid");
        let params = connection_params(&external, &defaults(), None, &Recorder).expect("maps");
        assert!(matches!(params.credentials(), Credentials::External));
        assert!(matches!(
            connection_params(&external, &defaults(), Some(Secret::new("x")), &Recorder),
            Err(ConnectError::PasswordNotUsed)
        ));
    }

    #[test]
    fn the_password_reaches_the_params_and_never_their_debug() {
        let marker = "reldex-test-marker-7f3c2a9e";
        let profile = Profile::create(details(host_port(ServiceTarget::ServiceName(
            "S".to_owned(),
        ))))
        .expect("valid");
        let params = connection_params(&profile, &defaults(), Some(Secret::new(marker)), &Recorder)
            .expect("maps");
        let Credentials::UserPassword { username, password } = params.credentials() else {
            panic!("expected user/password");
        };
        assert_eq!(username, "scott");
        assert_eq!(password.expose(), marker);
        assert!(!format!("{params:?}").contains(marker));
        assert!(!format!("{profile:?}").contains(marker));
    }

    #[test]
    fn a_binding_for_another_type_or_a_refusal_is_an_error() {
        struct Refuses;
        impl DriverBinding for Refuses {
            fn database_type(&self) -> DatabaseType {
                DatabaseType::Oracle
            }
            fn sid_endpoint(
                &self,
                _: &str,
                _: u16,
                _: &str,
                _: Transport,
            ) -> Result<Endpoint, DbError> {
                Err(DbError::new(ErrorKind::Configuration, "no SIDs here"))
            }
            fn extensions(&self, _: &DriverOptions<'_>) -> Result<Extensions, DbError> {
                Ok(Extensions::new())
            }
        }
        let profile =
            Profile::create(details(host_port(ServiceTarget::Sid("X".to_owned())))).expect("valid");
        match connection_params(&profile, &defaults(), Some(Secret::new("p")), &Refuses) {
            Err(ConnectError::Binding(error)) => {
                assert_eq!(error.kind(), ErrorKind::Configuration);
            }
            other => panic!("expected a binding refusal, got {other:?}"),
        }
    }

    #[test]
    fn statement_settings_arm_the_limit_and_the_fetch_hint() {
        let defaults = StatementSettings::resolve(&ResolveContext::new());
        let statement = defaults.apply(Statement::new("SELECT 1 FROM dual"));
        assert_eq!(statement.deadline(), Some(Duration::from_secs(600)));
        assert_eq!(statement.fetch_rows().map(NonZeroUsize::get), Some(1_000));
        assert_eq!(defaults.fetches_in_flight.value, 2);

        let mut app = SettingsLayer::<ApplicationScope>::new();
        app.set(FETCH_ROWS, 250).expect("allowed");
        let mut worksheet = SettingsLayer::<WorksheetScope>::new();
        worksheet
            .set(STATEMENT_TIME_LIMIT, TimeLimit::NoLimit)
            .expect("allowed");
        let settings = StatementSettings::resolve(
            &ResolveContext::new()
                .with_application(&app)
                .with_worksheet(&worksheet),
        );
        assert_eq!(settings.time_limit.source, Level::Worksheet);
        assert_eq!(settings.fetch_rows.source, Level::Application);
        let statement = settings.apply(Statement::new("BEGIN long_job; END;"));
        assert_eq!(statement.deadline(), None);
        assert_eq!(statement.fetch_rows().map(NonZeroUsize::get), Some(250));
    }

    #[test]
    fn server_output_is_off_by_default_and_carries_its_buffer_when_on() {
        let defaults = ServerOutputSettings::resolve(&ResolveContext::new());
        assert_eq!(defaults.setting(), ServerOutputSetting::Disabled);

        let mut worksheet = SettingsLayer::<WorksheetScope>::new();
        worksheet.set(SERVER_OUTPUT_ENABLED, true).expect("allowed");
        let mut profile = SettingsLayer::<ProfileScope>::new();
        profile
            .set(SERVER_OUTPUT_BUFFER, ByteLimit::Unlimited)
            .expect("allowed");
        let context = ResolveContext::new()
            .with_profile(&profile)
            .with_worksheet(&worksheet);
        let settings = ServerOutputSettings::resolve(&context);
        assert_eq!(
            settings.setting(),
            ServerOutputSetting::Enabled(ServerOutputBuffer::Unlimited)
        );
        assert_eq!(settings.enabled.source, Level::Worksheet);
        assert_eq!(settings.buffer.source, Level::Profile);

        let only_on =
            ServerOutputSettings::resolve(&ResolveContext::new().with_worksheet(&worksheet));
        assert_eq!(
            only_on.setting(),
            ServerOutputSetting::Enabled(ServerOutputBuffer::Bytes(
                NonZeroU32::new(1_000_000).expect("non-zero")
            ))
        );
    }
}
