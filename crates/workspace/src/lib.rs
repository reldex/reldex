//! Reldex local persistence — the core of `WorkspaceService`
//! (`ARCHITECTURE.md` §3): typed settings with three-level resolution and
//! provenance, connection profiles, and the SQLite file they live in.
//!
//! Decision record: `docs/decisions/0006-local-persistence-settings-profiles-sqlite.md`.
//!
//! # What is here
//!
//! - [`settings`] — the registry of every setting (id, value kind, built-in
//!   default, bounds, the levels it may be set at) and resolution:
//!   `effective = worksheet ?? profile ?? application ?? built-in`, reported
//!   with the [`Level`] it came from so a UI can say "inherited from
//!   profile".
//! - [`Profile`] — a saved connection (`SPEC.md` §17), identified by a random
//!   UUID ([`ProfileId`]) that is also its credential-store key.
//! - [`Store`] — one SQLite file, in WAL mode, with a forward-only schema
//!   migration path.
//!
//! # Where the file is
//!
//! [`Store::open_default`] uses [`store::default_store_path`]:
//! `%LOCALAPPDATA%\com.reldex.reldex\reldex.sqlite3` on Windows,
//! `$HOME/Library/Application Support/com.reldex.reldex/…` on macOS, and
//! `$XDG_DATA_HOME/com.reldex.reldex/…` (else `$HOME/.local/share/…`) on
//! other Unix. Only those environment variables are read, so they are also
//! how a user or a test points Reldex at another directory: set
//! `LOCALAPPDATA`, `HOME` or `XDG_DATA_HOME` (an absolute path; a relative
//! `XDG_DATA_HOME` is ignored, as the XDG specification requires). Android
//! and iOS have no such directory; the platform layer passes an explicit path
//! to [`Store::open`].
//! - [`connection_params`], [`StatementSettings`], [`ServerOutputSettings`]
//!   — pure functions that turn a profile and resolved settings into what
//!   `db-core` takes. No I/O.
//!
//! # No secret is ever written to SQLite
//!
//! Enforced by construction, not by care: no type this crate stores has a
//! field that can hold a password, and the schema has no column for one. A
//! profile records only whether the operating system's credential store
//! holds its password ([`PasswordStorage`]); the password travels from that
//! store (M2.10) straight into [`connection_params`] as a
//! [`reldex_db_driver_api::Secret`], which redacts itself in `Debug`. A
//! password pasted into an endpoint's free text is refused at validation
//! ([`CredentialPattern`]), and a connect string's `Debug` prints only its
//! length. `tests/no_secrets.rs` proves it on the bytes of a real file.
//!
//! # Threading
//!
//! Nothing here starts a thread. [`Store`] is `Send`, not `Sync`, and is
//! owned by the workspace service's thread — never the UI thread, because
//! every store call is disk I/O (`ARCHITECTURE.md` §6). See [`mod@store`].
//!
//! # Boundaries
//!
//! Vendor-neutral: the one database-type enumeration ([`DatabaseType`])
//! names which driver opens a profile, and everything a driver spells its
//! own way goes through a [`DriverBinding`] the composition root supplies.
//! Of Reldex's crates this one depends on `reldex-db-driver-api` only — not
//! on `db-core`, so `db-core` stays free of SQLite, and not on any driver;
//! its only other dependencies are `rusqlite` and `uuid`
//! (`tests/dependency_rules.rs` checks all of it).
//!
//! Query history (M4.10), workspace layout (M6.2), the metadata cache and
//! the credential store itself (M2.10) are not built here; the schema's
//! migration path is where the first two will be added.

mod connect;
mod ids;
mod profile;
pub mod settings;
pub mod store;
mod time;

pub use connect::{
    ConnectError, ConnectSettings, DriverBinding, DriverOptions, ServerOutputSettings,
    StatementSettings, connection_params,
};
pub use ids::{CredentialKey, IdError, ProfileId, WorksheetId};
pub use profile::{
    Authentication, CredentialPattern, DatabaseType, Environment, MAX_FIELD_BYTES, MAX_NAME_CHARS,
    PasswordStorage, Profile, ProfileDetails, ProfileEndpoint, ProfileError, ProfileField,
    ServiceTarget, TlsOptions, Transport,
};
pub use settings::{Level, ResolveContext, Resolved};
pub use store::{
    Loaded, RejectReason, RejectedRow, Scope, Store, StoreError, StoreOptions, StoreTable,
};
pub use time::UnixTimeMs;
