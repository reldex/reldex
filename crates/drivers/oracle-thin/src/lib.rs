//! Thin (pure-Rust, non-OCI) Oracle Database driver.
//!
//! This crate implements the `reldex-db-driver-api` contract for the default,
//! OCI-free connection path to Oracle Database 19c and later (`SPEC.md` §7,
//! `docs/architecture/ARCHITECTURE.md` §4). Vendor-specific code and types live
//! only here; nothing from the underlying Oracle crate appears in this crate's
//! public API, so `reldex-db-core` and the UI never see an Oracle type.
//!
//! ```no_run
//! use reldex_db_driver_api::{
//!     ConnectionParams, Credentials, DatabaseDriver, Endpoint, Secret, Statement,
//! };
//! use reldex_driver_oracle_thin::OracleThinDriver;
//!
//! # fn main() -> reldex_db_driver_api::DbResult<()> {
//! let driver = OracleThinDriver::new();
//! let mut connection = driver.connect(&ConnectionParams::new(
//!     Endpoint::HostPort {
//!         host: "127.0.0.1".to_owned(),
//!         port: 1521,
//!         service: "RELDEX".to_owned(),
//!     },
//!     Credentials::UserPassword {
//!         username: std::env::var("RELDEX_TEST_ORACLE_USER").unwrap_or_default(),
//!         password: Secret::new(std::env::var("RELDEX_TEST_ORACLE_PASSWORD").unwrap_or_default()),
//!     },
//! ))?;
//! let outcome = connection.execute(&Statement::new("SELECT 1 FROM dual"))?;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```
//!
//! # Dependencies (`AGENTS.md`, "Dependencies")
//!
//! **`oracledb`, pinned to `=26.0.0-beta.3`.** This is Oracle's own pure-Rust
//! thin driver (<https://github.com/oracle/rust-oracledb>, docs at
//! <https://oracle.github.io/rust-oracledb/>), adopted by **ADR-0001**
//! (`docs/decisions/0001-database-driver-strategy.md`). It speaks Oracle's TTC
//! protocol directly, so there is no Instant Client to ship, no `unsafe` FFI to
//! audit, and no `PATH`/`LD_LIBRARY_PATH` deployment problem — the three costs
//! ADR-0001 weighed the alternatives against. It is used under the Universal
//! Permissive License v1.0 / Apache 2.0.
//!
//! The pin is **exact** because the crate is pre-GA and its own documentation
//! says the API is subject to change; a caret range would let a beta bump break
//! the build silently. `26.0.0-beta.3` (published 2026-09-08) was the newest
//! version on crates.io when this crate was written. Upgrading is a deliberate
//! act: re-run the Phase 0 spikes, because several behaviours this wrapper
//! works around are undocumented internals rather than contracts.
//!
//! **Transport security.** `oracledb` reaches TLS (TCPS) through `rustls` 0.23,
//! whose default crypto provider is `aws-lc-rs`. That provider builds C and
//! assembly and therefore needs a C toolchain — MSVC on Windows, which the
//! Phase 0 machine has. It built without intervention, so the `ring` provider
//! was not needed; see `docs/exec-plans/active/phase-0-spike-results.md` for
//! the build measurement.
//!
//! TCPS **is** advertised in
//! [`Capabilities`](reldex_db_driver_api::Capabilities) as of spike S8, which
//! ran against a TLS listener added to the Phase 0 container
//! (`tools/oracle-test-db/startup/10_enable_tcps.sh`): TLS 1.2 with
//! `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`, certificate and host name verified,
//! against a **private** CA. [`TlsMode::Required`](reldex_db_driver_api::TlsMode::Required)
//! turns an [`Endpoint::HostPort`](reldex_db_driver_api::Endpoint::HostPort)
//! into a `tcps://` address and **refuses** a connect-string endpoint that does
//! not itself say `(PROTOCOL=TCPS)`, rather than opening a plaintext session for
//! a profile that requires TLS.
//!
//! Trusting a private issuer goes through [`EXT_WALLET_DIR`], which is the one
//! mechanism upstream offers: the directory's `ewallet.pem` is added to the root
//! store when it holds no private key. What is **not** available is the rest of
//! Oracle's TLS configuration surface — `SSL_SERVER_DN_MATCH` and
//! `SSL_SERVER_CERT_DN` are parsed from a descriptor and sent to the server, but
//! upstream's TLS layer never reads them (U-14); there is no mutual-TLS *and*
//! private-CA combination, because one `ewallet.pem` is read as one or the
//! other; and `orapki`'s own wallet files are not read at all. See
//! [`EXT_WALLET_DIR`] and upstream gaps U-12 to U-14 in the spike results.
//!
//! Because U-14 is a parameter being *ignored* rather than a feature being
//! absent, the two are not treated alike. A descriptor that sets
//! `SSL_SERVER_CERT_DN` asks for something this driver cannot deliver — the
//! server certificate's distinguished name pinned — so
//! [`connect`](reldex_db_driver_api::DatabaseDriver::connect) **refuses it**
//! before any socket is opened, rather than handing back a session with a
//! weaker guarantee than the profile configured;
//! [`EXT_ALLOW_UNENFORCED_SERVER_CERT_DN`] opts out of the refusal for a caller
//! who accepts that. A descriptor that sets `SSL_SERVER_DN_MATCH` is always
//! accepted, because host-name verification against `subjectAltName` is on
//! unconditionally and cannot be turned off, and is reported through a warning
//! so a profile that expected either answer is not left guessing.
//!
//! **The `rustls` crypto provider.** `rustls` resolves its default provider from
//! the compiled-in features, which works while exactly one is enabled — today,
//! `aws-lc-rs`. The moment a second one can be, that resolution becomes
//! ambiguous and the first handshake panics with "no process-level
//! `CryptoProvider` available": a run-time failure caused by a build-time
//! change, in a code path that only runs when a customer turns TLS on.
//! [`install_default_crypto_provider`] closes that hole, and
//! [`connect`](reldex_db_driver_api::DatabaseDriver::connect) calls it when TLS
//! is required. It installs **only** when nothing has installed one, so an
//! application that chose FIPS or a hardware backend keeps its choice; call it
//! yourself at start-up if you would rather the decision were explicit.
//!
//! # What this driver can and cannot do
//!
//! The capabilities it reports are deliberately conservative — every `true` is
//! backed by a spike in `docs/exec-plans/active/phase-0-spike-results.md`:
//!
//! - **Cancellation is
//!   [`PreArmedDeadline`](reldex_db_driver_api::CancelKind::PreArmedDeadline),
//!   not [`Native`](reldex_db_driver_api::CancelKind::Native).** `oracledb`
//!   holds one mutex across a whole round trip and exposes no break/interrupt
//!   call, so a running statement cannot be interrupted on request from another
//!   thread. A deadline armed *before* the call through
//!   [`Statement::with_deadline`](reldex_db_driver_api::Statement::with_deadline)
//!   does work, and
//!   [`request_cancel`](reldex_db_driver_api::CancelHandle::request_cancel)
//!   reports
//!   [`NotInterruptible`](reldex_db_driver_api::CancelOutcome::NotInterruptible)
//!   with the time remaining so the UI can say what will actually happen.
//!   `SPEC.md` §24.8 therefore cannot be met by this driver alone today.
//! - **The transaction state is not exact.** The upstream crate tracks whether
//!   a transaction is in progress but does not expose it, so this driver
//!   reports [`Unknown`](reldex_db_driver_api::TransactionState::Unknown) after
//!   anything that might have opened one, and only claims
//!   [`Inactive`](reldex_db_driver_api::TransactionState::Inactive) after
//!   connect, commit, rollback and DDL.
//! - **An error position is available for PL/SQL only.** This upstream version
//!   discards the server's SQL error offset; the ORA-06550 line and column are
//!   recovered by parsing the message text.
//! - **Auto-commit is off** and nothing in this crate commits implicitly. DDL
//!   still commits server-side, which is reported through
//!   [`committed_implicitly`](reldex_db_driver_api::ExecutionOutcome::committed_implicitly)
//!   rather than hidden.
//! - **`CREATE … TRIGGER` is rewritten so it can run at all.** `oracledb`
//!   reads `:NEW`, `:OLD` — any `:name` — in a trigger body as a bind
//!   placeholder and then demands a value for it, so the statement every other
//!   Oracle client accepts cannot be executed (upstream gap U-18). Such a
//!   statement is therefore submitted inside
//!   `BEGIN EXECUTE IMMEDIATE q'…'; END;`, **on by default**, always with a
//!   [`Warning`](reldex_db_driver_api::Warning) on the outcome carrying the
//!   exact text sent. It is still reported as
//!   [`StatementKind::Ddl`](reldex_db_driver_api::StatementKind::Ddl).
//!   [`EXT_REWRITE_TRIGGER_DDL`] turns it off, and the explanatory refusal
//!   comes back instead.
//! - **A connect is bounded, by this driver rather than by `oracledb`.**
//!   `oracledb` 26.0.0-beta.3 cannot bound one at all (upstream gap U-15), so
//!   [`connect`](reldex_db_driver_api::DatabaseDriver::connect) runs its
//!   blocking call on a helper thread and stops waiting at the limit:
//!   [`ConnectionParams::connect_timeout`](reldex_db_driver_api::ConnectionParams::connect_timeout)
//!   when the caller set one, otherwise [`DEFAULT_CONNECT_TIMEOUT`] (15 s), or
//!   no limit at all with [`EXT_CONNECT_TIMEOUT_UNBOUNDED`]. Nothing can
//!   *interrupt* the abandoned attempt, so it is left to finish on its own —
//!   but a session that opens after the limit is closed on that thread and
//!   **never** handed to the caller.
//!
//! # Values
//!
//! `NUMBER` is carried losslessly through the upstream type's canonical decimal
//! rendering into [`Number`](reldex_db_driver_api::Number) (40 significant
//! digits, the precision Oracle itself stores), never through `f64`. `DATE`,
//! `TIMESTAMP` and `TIMESTAMP WITH TIME ZONE` become
//! [`Timestamp`](reldex_db_driver_api::Timestamp) through its lenient
//! historical constructor, so a value the proleptic Gregorian calendar rejects
//! (a `1500-02-29` stored years ago, a BC date) is returned instead of failing
//! the fetch. `CLOB`, `NCLOB` and `BLOB` stay lazy locators streamed in bounded
//! chunks; a locator allocates its staging buffer on the first read, so a batch
//! of unopened locators costs nothing.
//!
//! A column whose type this contract cannot express becomes
//! [`Unsupported`](reldex_db_driver_api::ColumnData::Unsupported) text **where
//! `oracledb` can decode the value at all** — `ROWID`, `UROWID`, both
//! `INTERVAL`s and `TIMESTAMP WITH LOCAL TIME ZONE` — so one odd column of those
//! kinds never hides a whole table. `JSON`, `XMLTYPE`, `VECTOR`, object types
//! and `BFILE` are a different case: upstream's row deserializer has no branch
//! for them and fails the fetch, with nothing to render. Those columns are
//! refused **at describe time**, before any batch is delivered, so a result set
//! never dies part-way through; see "Known limitations".
//!
//! [`size_hint`](reldex_db_driver_api::LobStream::size_hint) is in bytes, as the
//! contract says, which is why a `CLOB` or `NCLOB` reports `None`: upstream
//! counts a character LOB in Oracle UCS-2 units, and passing that off as a byte
//! count would be wrong by up to a factor of four. A `BLOB` reports its real
//! size.
//!
//! A `TIMESTAMP WITH LOCAL TIME ZONE` is rendered without a zone suffix.
//! Upstream's own `Display` writes a trailing `Z` whenever the offset fields are
//! zero — which is how this type always arrives, the server having normalized it
//! to the database time zone — and that would assert UTC without the driver
//! having asked for `DBTIMEZONE` or the session zone. The column's native type
//! name says what the value means.
//!
//! # Known limitations
//!
//! Each of these is a defect in `oracledb` 26.0.0-beta.3 that this driver
//! contains by **refusing** rather than by risking wrong data or a dead process.
//! All are recorded with a reproduction in
//! `docs/exec-plans/active/phase-0-spike-results.md` §5, and all should be
//! re-checked on the next upstream version.
//!
//! - **`TIMESTAMP WITH TIME ZONE` is refused** (U-3, U-4). A value whose zone is
//!   a named region — `TIMESTAMP '2026-01-01 00:00:00 Asia/Bangkok'` — is
//!   decoded through a bare `todo!()`, and because the panic unwinds while the
//!   client mutex is held it aborts the **process**, so no wrapper can contain
//!   it. Region and offset encodings cannot be told apart before the value is
//!   decoded, and the decode happens inside the upstream round trip, so the
//!   refusal has to be per *column*: a query whose select list contains such a
//!   column, and an output bind declared with that type, both fail with
//!   [`Unsupported`](reldex_db_driver_api::ErrorKind::Unsupported) before
//!   anything is fetched. `TO_CHAR(c, '… TZR')` in the statement reads the value
//!   as text. The offset-only form does decode correctly, and
//!   [`EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE`] turns the refusal off for a caller
//!   that knows its data — at the price of the abort.
//! - **Some `NUMBER` values cannot be bound.** Both defects are in the same
//!   upstream encoder and the refusal set is derived from it rather than from
//!   the examples that exposed it. Upstream's encoder works on a digit count
//!   `n` (the significant digits, or the decimal exponent when trailing zeros
//!   pad an integer) and an index `d` (that exponent); a value is refused when
//!   `n > 40`, when `d` is **odd and positive** and `n == 40`, or when `d` is
//!   **odd and negative**.
//!     - Odd **negative** exponent (U-1): the encoder skips the alignment zero,
//!       because `-1 % 2` is `-1` in Rust, and the server silently stores a
//!       value ten times too large. `0.05`, `0.0005`, `1E-4` and half of all
//!       decimals below `0.1` are affected.
//!     - Odd **positive** exponent with 40 digits, or `n` above 40 (U-2): the
//!       encoder prepends its alignment zero, so 40 digits occupy 41 positions,
//!       and it reads `digits[40]` of a 40-byte array — the process aborts.
//!       "Magnitude ≥ 1E40" is **not** the criterion:
//!       `1.234567890123456789012345678901234567891` is the size of *one* and
//!       aborts, while `9.99E39` is larger than anything refused and encodes
//!       perfectly. The server cannot produce this shape itself (an odd `d`
//!       costs one of its 40 digit positions, so `SELECT 10/3 FROM dual`
//!       returns 39 digits); it comes from values a user types or the
//!       application computes.
//!
//!   Reading is exact in every case, verified to 126 digits; only binding is
//!   affected, and a refused value can always be written as a literal.
//! - **A running statement cannot be interrupted** (U-10); see the capability
//!   note above. The deadline that *can* be armed is upstream's per-round-trip
//!   socket timeout, so it bounds each round trip and not a whole multi-batch
//!   fetch, and it lives on the connection rather than on a statement:
//!   [`CancelHandle::request_cancel`](reldex_db_driver_api::CancelHandle::request_cancel)
//!   reports no remaining time while a result set is open, and a statement that
//!   changes the connection's limit while one is open says so through a warning.
//! - **A cursor-typed result column** (`SELECT CURSOR(…) …`) is refused rather
//!   than silently dropped (ADR-0002 lead decision 3). A `REF CURSOR` through an
//!   *output bind* is fully supported.
//! - **`JSON`, `XMLTYPE`, `VECTOR`, object types and `BFILE` columns are refused
//!   on the describe.** `oracledb` 26.0.0-beta.3's `DbValue::from_response` has
//!   no branch for them, so the fetch itself fails and there is no value to
//!   render as text. Refusing before the first batch keeps the failure from
//!   arriving after part of a result set has been delivered. (`SPEC.md` §8 lists
//!   JSON: on Oracle Database 19c, which Phase 0 tests against, there is no
//!   native `JSON` column type — JSON is stored in `VARCHAR2`, `CLOB` or `BLOB`
//!   with an `IS JSON` constraint, and those all work. The native type is 21c
//!   and later, and is untested here.)
//! - **Multi-row `RETURNING … INTO` is refused.** A single-row `RETURNING` is
//!   read from upstream's `returned_data`, which is where it lives; more than
//!   one returned row cannot be expressed as one value per output bind, so it is
//!   reported rather than truncated.
//! - **An output bind the server does not agree about is refused.** The wrapper
//!   numbers output slots from the caller's declared directions, while the row
//!   it reads comes from the server's describe. A parameter declared `In` that
//!   the PL/SQL signature makes `IN OUT` shifts every later slot — silently,
//!   when the types happen to match — so the two counts are compared and a
//!   mismatch fails loudly.
//! - **A rewritten trigger is limited to 32767 bytes, and its error positions
//!   move.** The rewrite above puts the DDL in a PL/SQL string literal, whose
//!   limit is 32767 **bytes** (verified against the live database:
//!   32768 is `PLS-00172: string literal too long`). A longer trigger is
//!   refused with that reason rather than sent to fail obscurely. A trigger
//!   that compiles with errors is turned back into the success plus
//!   [`CompiledWithErrors`](reldex_db_driver_api::WarningKind::CompiledWithErrors)
//!   warning a direct `CREATE` would have produced — the object really is
//!   created, which is what `ORA-24344` means — and the one row the wrapper
//!   block reports as affected is dropped, because the DDL affected none. Any
//!   *syntax* error's position, though, now refers to the wrapper block, and
//!   that is the one thing the rewrite cannot preserve (`SPEC.md` §24.14).
//! - **A connect that runs out of time costs a thread until it finishes**
//!   (U-15). The wait is bounded, the attempt is not: `oracledb` offers no way
//!   to cancel a connect in progress, so the helper thread that is carrying it
//!   lives until upstream's own call returns — which, against a link that
//!   accepts and then says nothing, may be never (U-17). One thread per
//!   abandoned attempt, so the cost is bounded by how often a user retries. A
//!   session that arrives after the limit is closed on that thread and never
//!   adopted, which is what makes the timeout safe rather than merely fast.
//! - **`Endpoint::HostPort` accepts plain names only.** `Config`'s connect
//!   string is also where a full TNS descriptor goes, so a host beginning with
//!   `(` would turn an Easy Connect target into a descriptor pointing elsewhere.
//!   The host and service name are validated; descriptors go through
//!   [`Endpoint::ConnectString`](reldex_db_driver_api::Endpoint::ConnectString).
//!
//! # Threading
//!
//! Everything here is blocking and single-threaded per connection, as ADR-0002
//! requires: `reldex-db-core` owns one worker thread per session and every call
//! on a connection happens on it. There are two exceptions, both deliberate:
//! [`cancel_handle`](reldex_db_driver_api::DatabaseConnection::cancel_handle),
//! whose handle is `Send + Sync` and never blocks on the connection's own lock;
//! and [`connect`](reldex_db_driver_api::DatabaseDriver::connect), which runs
//! upstream's blocking connect on a short-lived helper thread so the wait can
//! be bounded (U-15). That thread touches nothing but the connection it is
//! opening, and either hands it over or closes it itself, so the "one thread
//! owns one connection" rule is preserved rather than bent.

mod binds;
mod classify;
mod conn;
mod connect_timeout;
mod cursor;
mod descriptor;
mod error;
mod lob;
mod rewrite;
mod value;

pub use conn::{
    EXT_ALLOW_TIMESTAMP_WITH_TIME_ZONE, EXT_ALLOW_UNENFORCED_SERVER_CERT_DN,
    EXT_REWRITE_TRIGGER_DDL, EXT_STATEMENT_CACHE_SIZE, EXT_WALLET_DIR, EXT_WALLET_PASSWORD,
    OracleThinDriver, install_default_crypto_provider,
};
pub use connect_timeout::{DEFAULT_CONNECT_TIMEOUT, EXT_CONNECT_TIMEOUT_UNBOUNDED};
