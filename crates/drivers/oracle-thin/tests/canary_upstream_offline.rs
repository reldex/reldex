//! Upstream canaries that need **no database**, plus the version tripwire.
//!
//! # What a canary is, and why it fails when the news is good
//!
//! The spikes in `s*.rs` test *this wrapper's* behaviour. These tests do the
//! opposite: each one drives **raw `oracledb`** and asserts the defect
//! `docs/exec-plans/active/phase-0-spike-results.md` §5 recorded is still
//! there. So a canary that fails is telling you upstream fixed something and a
//! guard in this crate can be reconsidered — the failure message names which
//! U-number, which upstream issue, and which guard.
//!
//! Three outcomes, and the message says which:
//!
//! - **passes** — the defect is still present, the guard must stay;
//! - **fails** — the defect looks fixed; read the message, re-check by hand,
//!   then remove the guard it names and update this file;
//! - **fails with "changed"** — the behaviour moved but not to the fixed
//!   shape. Re-derive the guard before touching it.
//!
//! This file runs in a plain `cargo test --workspace`, with no database and no
//! feature flag. The canaries that need one live in `canary_upstream_live.rs`.
//! `docs/exec-plans/active/oracledb-upgrade-checklist.md` is the procedure
//! these two files exist to make enforceable.

#![allow(
    clippy::print_stdout,
    reason = "a canary's observation is something a human reads from the test output"
)]

use std::marker::PhantomData;

/// The `oracledb` version every canary in this crate was derived from.
///
/// Bumping the pin without updating this constant is what the tripwire below
/// exists to stop.
const EXPECTED_ORACLEDB_VERSION: &str = "26.0.0-beta.3";

/// What to do when a canary's premise no longer holds.
const UPGRADE_PROCEDURE: &str =
    "follow docs/exec-plans/active/oracledb-upgrade-checklist.md before changing any guard";

/// Prints one observation, in the shape the spikes use.
fn observation(text: impl std::fmt::Display) {
    println!("OBSERVATION {text}");
}

// ---------------------------------------------------------------------------
// The version tripwire
// ---------------------------------------------------------------------------

/// The locked version of `oracledb` and the requirement that pinned it both
/// still say [`EXPECTED_ORACLEDB_VERSION`].
///
/// This is the test that forces the procedure: every canary below asserts a
/// defect *of a particular upstream version*, so an upgrade that leaves them
/// unexamined would be asserting the wrong thing. Failing here is the first
/// thing an upgrade sees.
#[test]
fn the_pinned_oracledb_version_is_the_one_these_canaries_were_derived_from() {
    let locked = locked_version(include_str!("../../../../Cargo.lock"), "oracledb")
        .expect("Cargo.lock must contain a package entry for oracledb");
    let required = manifest_requirements(include_str!("../Cargo.toml"), "oracledb");

    assert_eq!(
        locked, EXPECTED_ORACLEDB_VERSION,
        "oracledb was upgraded ({EXPECTED_ORACLEDB_VERSION} -> {locked}). Run the whole \
         canary suite before anything else:\n  \
         cargo test -p reldex-driver-oracle-thin --test canary_upstream_offline\n  \
         tools/oracle-test-db/run-it.ps1 canary_upstream_live -- --test-threads=1\n\
         then update EXPECTED_ORACLEDB_VERSION here, every `oracledb 26.0.0-beta.3` \
         mentioned in src/, and docs/decisions/0001-database-driver-strategy.md. \
         {UPGRADE_PROCEDURE}"
    );
    assert_eq!(
        required,
        vec![format!("={EXPECTED_ORACLEDB_VERSION}"); 2],
        "both the dependency and the dev-dependency on oracledb must stay pinned to an \
         exact version (ADR-0001: the crate is pre-GA and its API is subject to change)"
    );
    observation(format!(
        "oracledb locked at {locked}, required as {required:?}"
    ));
}

/// The version `Cargo.lock` records for one package.
fn locked_version(lock: &str, package: &str) -> Option<String> {
    lock.split("[[package]]").find_map(|block| {
        let value = |key: &str| {
            block
                .lines()
                .find_map(|line| line.trim().strip_prefix(key))
                .map(|rest| rest.trim().trim_matches('"').to_owned())
        };
        (value("name =").as_deref() == Some(package)).then(|| value("version ="))?
    })
}

/// Every `oracledb = "…"` requirement in this crate's manifest, in file order.
///
/// There are two — the dependency and the dev-dependency that the canaries and
/// spike S4 use — and both have to move together.
fn manifest_requirements(manifest: &str, package: &str) -> Vec<String> {
    let prefix = format!("{package} =");
    manifest
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .map(|rest| rest.trim().trim_matches('"').to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// U-9, U-11 — trait implementations that are absent
// ---------------------------------------------------------------------------

/// Carries a type into the probe below without needing a value of it.
struct Probe<T>(PhantomData<T>);

/// Answers "does `T` implement the trait?" at **run time**, for a trait the
/// answer is currently *no* for.
///
/// The obvious `fn assert_impl<T: Trait>()` cannot be used: when the answer is
/// no it fails the *build*, and a canary that breaks the build tells nobody
/// which U-number moved. This is the standard autoref trick — method lookup
/// tries the by-value receiver `Probe<T>` first and only reaches the autoref'd
/// `&Probe<T>` when the bound on the first does not hold, and a candidate whose
/// bound fails is discarded rather than reported. Both arms compile either way,
/// which is the whole point.
#[allow(
    dead_code,
    reason = "unused exactly while the bound does not hold, which is the answer"
)]
trait ImplementsStdError {
    /// `true` — reachable only while `T: std::error::Error`.
    fn implements_std_error(self) -> bool;
}

impl<T: std::error::Error> ImplementsStdError for Probe<T> {
    fn implements_std_error(self) -> bool {
        true
    }
}

/// The fallback arm of [`ImplementsStdError`]; see its documentation.
trait DoesNotImplementStdError {
    /// `false` — chosen when the bound on the by-value arm does not hold.
    fn implements_std_error(self) -> bool;
}

impl<T> DoesNotImplementStdError for &Probe<T> {
    fn implements_std_error(self) -> bool {
        false
    }
}

/// The same trick for `Debug`; see [`ImplementsStdError`].
#[allow(
    dead_code,
    reason = "unused exactly while the bound does not hold, which is the answer"
)]
trait ImplementsDebug {
    /// `true` — reachable only while `T: Debug`.
    fn implements_debug(self) -> bool;
}

impl<T: std::fmt::Debug> ImplementsDebug for Probe<T> {
    fn implements_debug(self) -> bool {
        true
    }
}

/// The fallback arm of [`ImplementsDebug`]; see [`ImplementsStdError`].
trait DoesNotImplementDebug {
    /// `false` — chosen when the bound on the by-value arm does not hold.
    fn implements_debug(self) -> bool;
}

impl<T> DoesNotImplementDebug for &Probe<T> {
    fn implements_debug(self) -> bool {
        false
    }
}

/// U-9: `oracledb::Error` still carries no `std::error::Error` implementation.
#[test]
fn u9_the_upstream_error_type_still_does_not_implement_the_standard_error_trait() {
    let implemented = Probe::<oracledb::Error>(PhantomData).implements_std_error();
    assert!(
        !implemented,
        "U-9 appears FIXED upstream (not submitted as an issue): oracledb::Error now \
         implements std::error::Error, so it can be attached with DbError::with_source. \
         Re-evaluate error.rs, which preserves the upstream text in NativeError instead \
         of chaining a source. {UPGRADE_PROCEDURE}"
    );
    observation("U-9 still present: oracledb::Error is Debug + Display only");
}

/// U-11 (second bullet): `oracledb::Config` still carries no `Debug`, so a
/// `Result<Config, _>` cannot be `expect_err`ed.
#[test]
fn u11_the_upstream_config_type_still_does_not_implement_debug() {
    let implemented = Probe::<oracledb::Config>(PhantomData).implements_debug();
    assert!(
        !implemented,
        "U-11 appears FIXED upstream (not submitted as an issue): oracledb::Config now \
         implements Debug, so a Result<Config, _> can be unwrapped in tests directly. \
         Nothing in src/ depends on this; drop the U-11 row from the upgrade checklist. \
         {UPGRADE_PROCEDURE}"
    );
    observation("U-11 still present: oracledb::Config does not implement Debug");
}

// ---------------------------------------------------------------------------
// U-12 — a wallet directory named in a descriptor never reaches the TLS layer
// ---------------------------------------------------------------------------

/// U-12: a wallet directory named in a descriptor never reaches the field the
/// TLS layer trusts roots from — `Config::wallet_location()` stays empty
/// whichever way it is spelled.
///
/// Two spellings, because the parser and the writer disagree about which one
/// this is (a detail §5's U-12 gets wrong; see the checklist):
///
/// - `WALLET_LOCATION` is the only key `process_security_nodes` has an arm
///   for. It is parsed, and written back out under the **other** spelling;
/// - `MY_WALLET_DIRECTORY` — the spelling `orapki`, `tnsnames.ora` and every
///   other Oracle client use, and the one this crate emits — has **no** arm at
///   all and is dropped in silence.
///
/// Neither reaches `Config::wallet_location`, which is what `transport.rs`
/// reads, so both leave a caller believing they configured trust when they did
/// not.
#[test]
fn u12_a_wallet_directory_named_in_a_descriptor_still_never_reaches_the_tls_layer() {
    const WALLET: &str = "/etc/reldex/wallet";
    let descriptor_with = |key: &str| {
        oracledb::Config::default()
            .set_connect_string(&format!(
                "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.example.com)(PORT=2484))\
                 (CONNECT_DATA=(SERVICE_NAME=RELDEX))(SECURITY=({key}={WALLET})))"
            ))
            .expect("a descriptor naming a wallet directory is a valid connect string")
    };

    let parsed = descriptor_with("WALLET_LOCATION");
    let rebuilt = parsed.get_connect_descriptor();
    assert!(
        rebuilt.contains(&format!("MY_WALLET_DIRECTORY={WALLET}")),
        "U-12 CHANGED in an unexpected way (oracle/rust-oracledb#25): a descriptor's \
         WALLET_LOCATION is no longer echoed back as MY_WALLET_DIRECTORY, so this canary \
         no longer proves the parser sees it. Re-derive it from \
         config/connect_options.rs before trusting lib.rs::EXT_WALLET_DIR. Descriptor \
         was: {rebuilt}"
    );
    assert_eq!(
        parsed.wallet_location(),
        None,
        "U-12 appears FIXED upstream (oracle/rust-oracledb#25): a descriptor's \
         WALLET_LOCATION now reaches Config::wallet_location, which is the field the TLS \
         layer trusts roots from. Re-evaluate lib.rs::EXT_WALLET_DIR — a caller may now \
         be able to name the wallet in the connect string instead of in an extension. \
         {UPGRADE_PROCEDURE}"
    );

    let dropped = descriptor_with("MY_WALLET_DIRECTORY");
    let rebuilt = dropped.get_connect_descriptor();
    assert!(
        !rebuilt.contains(WALLET),
        "U-12 appears FIXED upstream (oracle/rust-oracledb#25): MY_WALLET_DIRECTORY — the \
         spelling every other Oracle client uses, and the one this crate itself emits — \
         is now understood by the descriptor parser. Re-evaluate lib.rs::EXT_WALLET_DIR \
         and the S8 limits in docs/exec-plans/active/phase-0-spike-results.md. \
         Descriptor was: {rebuilt}. {UPGRADE_PROCEDURE}"
    );
    assert_eq!(
        dropped.wallet_location(),
        None,
        "U-12 appears FIXED upstream (oracle/rust-oracledb#25): a descriptor's \
         MY_WALLET_DIRECTORY now reaches Config::wallet_location. Re-evaluate \
         lib.rs::EXT_WALLET_DIR. {UPGRADE_PROCEDURE}"
    );
    observation(
        "U-12 still present: WALLET_LOCATION is parsed and echoed back as \
         MY_WALLET_DIRECTORY, MY_WALLET_DIRECTORY itself is dropped, and neither reaches \
         Config::wallet_location",
    );
}

// ---------------------------------------------------------------------------
// U-15 — a connect cannot be bounded in time, by any documented route
// ---------------------------------------------------------------------------

/// U-15: neither a full descriptor nor Easy Connect can ask for a bounded
/// connect. The descriptor parser has no arm for any of the three spellings,
/// and the Easy Connect parser stops at the `?` and drops the rest of the
/// string without a word.
///
/// `RETRY_COUNT` is asserted alongside because it *is* honoured: it proves the
/// descriptor survived the round trip, so a missing timeout is the defect and
/// not a parser that gave up.
#[test]
fn u15_a_connect_string_still_cannot_ask_for_a_bounded_connect() {
    let descriptor_form = oracledb::Config::default()
        .set_connect_string(
            "(DESCRIPTION=(RETRY_COUNT=3)(TRANSPORT_CONNECT_TIMEOUT=5)(TCP_CONNECT_TIMEOUT=5)\
             (CONNECT_TIMEOUT=5)(ADDRESS=(PROTOCOL=TCP)(HOST=db.example.com)(PORT=1521))\
             (CONNECT_DATA=(SERVICE_NAME=RELDEX)))",
        )
        .expect("a descriptor naming the connect timeouts is accepted")
        .get_connect_descriptor();

    assert!(
        descriptor_form.contains("RETRY_COUNT=3"),
        "U-15 CHANGED in an unexpected way (Issue F, drafted): the descriptor parser no \
         longer round-trips RETRY_COUNT either, so this canary cannot tell a dropped \
         timeout from a dropped descriptor. Re-derive it. Descriptor was: {descriptor_form}"
    );
    assert!(
        !descriptor_form.to_uppercase().contains("CONNECT_TIMEOUT"),
        "U-15 appears FIXED upstream (Issue F, drafted — not yet submitted): a descriptor \
         can now name a connect timeout and it survives the parse. Re-evaluate conn.rs, \
         which ignores ConnectionParams::connect_timeout() outright (§7 C-5), and the \
         connect-timeout row of ADR-0001. Descriptor was: {descriptor_form}. \
         {UPGRADE_PROCEDURE}"
    );

    // The Easy Connect Plus form of the same request. The parser reads the
    // service name up to the `?`, returns success, and never looks at the rest
    // of the string — so a caller gets no error and no timeout either.
    let easy_connect_form = oracledb::Config::default()
        .set_connect_string("//db.example.com:1521/RELDEX?transport_connect_timeout=5")
        .expect("an Easy Connect string with a query part is accepted")
        .get_connect_descriptor();

    assert!(
        easy_connect_form.contains("SERVICE_NAME=RELDEX"),
        "U-15 CHANGED in an unexpected way: the Easy Connect parser no longer reads the \
         service name from a string that carries a query part. Re-derive this canary. \
         Descriptor was: {easy_connect_form}"
    );
    assert!(
        !easy_connect_form.to_uppercase().contains("CONNECT_TIMEOUT"),
        "U-15 appears FIXED upstream (Issue F, drafted — not yet submitted): the Easy \
         Connect parser now reads the `?key=value` part instead of discarding it. Besides \
         conn.rs's ignored connect_timeout, re-check every other Easy Connect Plus \
         parameter Reldex may now pass through. Descriptor was: {easy_connect_form}. \
         {UPGRADE_PROCEDURE}"
    );
    observation(
        "U-15 still present: TRANSPORT_CONNECT_TIMEOUT / TCP_CONNECT_TIMEOUT / \
         CONNECT_TIMEOUT are dropped by the descriptor parser, and the Easy Connect \
         `?query` part is discarded without an error",
    );
}

// ---------------------------------------------------------------------------
// U-16 — every socket timeout is reported as "your call timeout expired"
// ---------------------------------------------------------------------------

/// U-16: `From<std::io::Error> for oracledb::Error` maps both `TimedOut` and
/// `WouldBlock` onto `CallTimeoutExceeded` and drops the cause, so a TCP
/// connect the operating system gave up on is reported as a deadline the
/// caller set — about a session that never existed.
#[test]
fn u16_a_socket_timeout_is_still_reported_as_an_expired_call_timeout() {
    // The text a Windows connect failure carries; it is what has to survive
    // for the cause to be considered preserved.
    const OS_TEXT: &str = "connection attempt failed (os error 10060)";

    for io_kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
        let error = oracledb::Error::from(std::io::Error::new(io_kind, OS_TEXT));
        assert_eq!(
            *error.kind(),
            oracledb::ErrorKind::CallTimeoutExceeded,
            "U-16 appears FIXED upstream (Issue F, drafted — not yet submitted): a \
             std::io::ErrorKind::{io_kind:?} is no longer classified as \
             CallTimeoutExceeded. Re-evaluate error.rs::map_connect, which reclassifies \
             that kind back to ErrorKind::Connection during connect. Error was: {error}. \
             {UPGRADE_PROCEDURE}"
        );
        assert!(
            !error.to_string().contains("10060"),
            "U-16 is PARTLY FIXED upstream (Issue F, drafted — not yet submitted): the \
             underlying io::Error is now kept as the cause, so the operating system's own \
             text survives. Re-evaluate error.rs::map_connect's message, which currently \
             has to say the wait was the operating system's because nothing else is left. \
             Error was: {error}. {UPGRADE_PROCEDURE}"
        );
    }
    observation(
        "U-16 still present: TimedOut and WouldBlock both become CallTimeoutExceeded \
         with the cause discarded",
    );
}
