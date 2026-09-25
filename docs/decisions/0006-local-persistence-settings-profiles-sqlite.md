# 0006 — Local persistence: settings, profiles and the SQLite store

**Status:** Accepted — the choices below implement owner decisions already taken: 2026-09-19
(every default the lead chose is user-configurable, and each setting says at which level it lives;
the connect-timeout, time-limit, trigger-rewrite and fetch-size defaults) and 2026-09-20
(`docs/exec-plans/active/phase-1.md` §C.3 items 5 app id `com.reldex.reldex`, 7 secrets only in the
OS credential store with no plaintext fallback, 8 one SQLite file via `rusqlite` with bundled
SQLite, 9 no telemetry). Nothing here needed a new owner decision.
**Date:** 2026-09-24; revised 2026-09-25 with the independent review's follow-ups (credential
guard in endpoints, production flag, SID builder moved into the driver, store hardening).
**Task:** M2.9 ★ (`phase-1.md` §C.2)

## Context

`SPEC.md` §17 defines connection profiles (database type, environment, host/port,
service/SID/descriptor, authentication, session options, display options) and requires passwords in
platform secure storage. §20 lists what local SQLite may hold, starting with "profiles without
plaintext secrets" and "settings". §10 makes the per-statement time limit configurable at three
levels — application, connection profile, worksheet — including "no limit" with its consequence
stated. `ARCHITECTURE.md` §3 names the owner of all of this, `WorkspaceService` ("non-transactional
workspace, profiles, history, settings, layout state"), and §13 item 9 leaves the credential
abstraction to M2.10.

The M2.9 acceptance criteria: `effective = worksheet ?? profile ?? application ?? built-in` with the
source reported; a truth-table test; a schema migration path; **no secret ever written to SQLite**.
Downstream consumers are fixed by the plan: M2.10 (credential store keyed by profile UUID), M2.11
(FFI), M3.2 (connection manager), M3.6 (settings UI showing "inherited from profile"), M4.6 (the
three-level time-limit control incl. "no limit"), M4.10 (query history per profile), M6.2
(workspace persistence, non-transactional state only).

Binding constraints from `AGENTS.md`: product/core layers vendor-neutral; explicit typed APIs, not
stringly cross-layer protocols; no database or network I/O — and by the same reasoning no disk I/O
— on the UI thread; no credential in logs; no production dependency without a documented reason.

## Decision

### P1 — A crate of its own: `crates/workspace` (`reldex-workspace`)

The plan row says "db-core workspace/settings module + schema". It is a separate crate instead,
depending on `reldex-db-driver-api` only — not on `reldex-db-core`, and on no driver:

- **`db-core` stays free of SQLite.** `db-core` is the session/worker layer: one thread per
  session, every call on the query path. The store is the opposite — one file, one service
  thread, never on the query path. Folding it in would give every `db-core` consumer (the device
  harness, the FFI smoke build, mobile) a C-compiled SQLite it does not use, and would make
  `db-core`'s dependency rule ("`reldex-db-driver-api` only", `crates/db-core/src/lib.rs`,
  enforced by `tests/dependency_rules.rs`) false.
- **What the two share is data, not code.** A profile and its resolved settings become a
  `ConnectionParams` and a `Statement`'s options — both `db-driver-api` types. Nothing in the
  mapping needs a session.
- **The composition root joins them.** The `WorkspaceService` the UI talks to (M2.11/M3) owns a
  `Store` and a `SessionRegistry` side by side; neither needs the other's crate.

`ARCHITECTURE.md` §2 draws "workspace" inside the Core box; it still is — vendor-neutral,
UI-independent Rust — as a crate of its own beside `db-core`, not inside it.
`tests/dependency_rules.rs` checks the rule both ways.

### P2 — Settings: a typed registry, three levels, provenance

**Registry.** Every setting is a `SettingId` variant with a static `SettingDescriptor`: stable
storage key (SQLite only and crate-private; it never crosses a layer — the FFI, M2.11, names a
setting by a numeric id of its own), group, value kind, built-in default, the set of
levels it may be set at, numeric bounds, whether "no limit" is accepted and — if so — *which*
consequence a UI must state (`NoLimitConsequence`), and when a change takes effect
(`NextConnection` / `NextStatement`). Code that knows which setting it means uses typed handles —
`CONNECT_TIMEOUT: Setting<TimeLimit>` — whose value type is checked against the descriptor's kind
**at compile time** (`Setting::new` is a `const fn` that asserts it). Value kinds: `bool`,
`TimeLimit { Seconds(NonZeroU32) | NoLimit }`, bounded `u32` counts, and
`ByteLimit { Bytes(NonZeroU32) | Unlimited }`. "No limit" is a value, never an absence: an absent
value inherits.

| Setting (`storage key`) | Default | Levels | Bounds | "No limit" | Takes effect |
| --- | --- | --- | --- | --- | --- |
| `connection.connect_timeout` | 15 s | application, profile | 1–3,600 s | yes — connect may wait forever | next connection |
| `connection.rewrite_trigger_ddl` | on | application, profile | — | — | next connection |
| `execution.statement_time_limit` | 600 s | application, profile, worksheet | 1–86,400 s | yes — only disconnecting ends a hung statement | next statement |
| `results.fetch_rows` | 1,000 | application, profile, worksheet | 1–100,000 | — | next statement |
| `results.fetches_in_flight` | 2 | application | 1–8 | — | next result |
| `server_output.enabled` | off | application, profile, worksheet | — | — | next statement |
| `server_output.buffer` | 1,000,000 bytes | application, profile, worksheet | 2,000 B–1 GiB | yes — server buffers without limit | next statement |

Where each number comes from: 15 s and "can be off", 600 s and "no limit", the trigger rewrite on
with a per-connection off switch — owner decisions 2026-09-19. The connect-timeout bound is the
Oracle thin driver's own `MAX_CONNECT_TIMEOUT` (a larger setting would be silently capped there;
`the_registry_agrees_with_the_drivers_own_connect_timeout_constants` checks both constants).
1,000 rows / 2 in flight — spike S15's sweep, measured at zero network latency; the owner's
sign-off on the number is still open (§C.3 item 10, M5.6), and it is a setting either way. Server
output off — M2.7 (a round trip per statement when on). The 1,000,000-byte buffer is the lead's
default: bounded, so one runaway loop cannot make a statement's drain unbounded while the drain
itself has no bound (M2.13); "unlimited", which is SQL\*Plus's choice, is one setting away. Its
lower bound, 2,000 bytes, is the smallest buffer the first driver's server honours — a smaller
request is silently raised to it (measured in M2.7) — so the setting never promises a limit that
does not hold; the upper bound is the setting's own, a driver clamps to what its server accepts and
reports the size in force (ADR-0002 T1).
`fetches_in_flight` is application-only because it tunes the one result pipeline a process has,
not a connection — **accepted** as such by the review; widening a setting's levels later is a
compatible change.

**Resolution.** `ResolveContext::new().with_application(&a).with_profile(&p).with_worksheet(&w)`
then `resolve(SETTING) -> Resolved<T> { value, source: Level }`, where
`effective = worksheet ?? profile ?? application ?? built-in`. Layers are typed by their level
(`SettingsLayer<ProfileScope>` …), so a worksheet's layer cannot be passed as a profile's. A level
the setting does not allow is excluded three times over: `SettingsLayer::set` and the store refuse
the write with `SettingError::LevelNotAllowed`; loading rejects (and reports) such a row; and
resolution skips it even if one got in. Provenance is where the value came from, not whether it
differs: a profile that sets the default value is reported as "from profile".

**Session and display options.** `SPEC.md` §17's per-profile session and display options *are* the
profile-level setting overrides — not extra profile fields — so they resolve and report provenance
like everything else. No display option is registered yet: none has been decided, and they arrive
with the result grid (M5). Adding a setting needs no schema migration.

### P3 — Profiles

`Profile { id: ProfileId, details: ProfileDetails, created_at, modified_at }`, `ProfileDetails`
being everything the user edits:

- `id` — a random (v4) UUID, never reused, so a credential keyed by it can never attach to another
  profile. Public surface: 16 raw bytes (what the C ABI will carry) and the canonical text form;
  the `uuid` type does not leak.
- `database: DatabaseType` — `#[non_exhaustive]`, `Oracle` only today. Naming the vendor here is the
  technical-compatibility use `AGENTS.md` allows; nothing vendor-specific *happens* in the crate.
- `environment` — `SPEC.md` §17's six: Development, Test, UAT, Staging, Production, Custom(label).
- `treat_as_production: bool` — whether the production indicator shows, the one thing M3.4 reads
  (never the enum). Validation ties it to the environment: always `true` for `Production`, always
  `false` for the other named environments, the user's explicit choice for `Custom` — a custom
  label is never guessed at. `ProfileDetails::new` starts it at `Environment::production_by_default`.
- `endpoint` — `HostPort { host, port, target: ServiceName | Sid }` or `ConnectString(text)`, typed
  variants rather than one string. `Debug` of a connect string prints only its length
  (`ConnectString(<redacted, N bytes>)`): it is the one field whose content the crate cannot vouch
  for.
- `authentication` — `Password { username, storage: CredentialStore | PromptEachTime }` or
  `External`. **There is no password field anywhere.**
- `role` — `SessionRole` (normal, SYSDBA, SYSOPER).
- `tls` — `transport: Plain | Tls`, `ca_directory` (a directory of CA certificates in PEM, the
  private-CA mechanism of spike S8), and `allow_unenforced_certificate_pin` — the C-6 guard's
  opt-out, off by default.

Validation is shallow on purpose — non-empty, bounded length, no control characters (line breaks
allowed only in a connect string), port ≠ 0, CA path valid Unicode, the production flag's rule —
because whether a host resolves or a descriptor parses is the driver's to say, at connect time.
The one deeper check is the credential guard in the endpoint's free text (P7).

Every enum a UI matches on — `Level`, `Environment`, `ServiceTarget`, `PasswordStorage`,
`Transport`, `Scope`, `CredentialPattern`, and the error enums — is `#[non_exhaustive]`, so adding
a variant is not a breaking change for M3.

### P4 — Mapping to `db-core`: pure functions and a driver binding

- `connection_params(&Profile, &ConnectSettings, Option<Secret>, &dyn DriverBinding)
  -> Result<ConnectionParams, ConnectError>`.
- `StatementSettings::resolve(ctx).apply(Statement)` arms `with_deadline` (none for "no limit") and
  `with_fetch_rows`.
- `ServerOutputSettings::resolve(ctx).setting()` is the `ServerOutputSetting` to send with
  `set_server_output` (M2.7); the session answers with the setting in force (ADR-0002 T1).

No I/O, no clock, no threads in any of them.

**The driver binding.** Most of a profile maps onto `ConnectionParams`' typed, vendor-neutral
fields (endpoint, credentials, role, TLS mode, connect timeout). A residue cannot: a SID has no
neutral endpoint shape (Easy Connect cannot name one), and "no connect limit", the trigger-rewrite
switch, the CA directory and the pin opt-out are driver extensions keyed by the driver's own
constants (ADR-0002 D7). Rather than copy vendor keys and descriptor syntax into a vendor-neutral
crate — or depend on a concrete driver from one — that residue goes through a small trait:

```rust
pub trait DriverBinding {
    fn database_type(&self) -> DatabaseType;
    fn sid_endpoint(&self, host: &str, port: u16, sid: &str, transport: Transport)
        -> Result<Endpoint, DbError>;
    fn extensions(&self, options: &DriverOptions<'_>) -> Result<Extensions, DbError>;
}
```

implemented once per database type by the **composition root** — the only place `ARCHITECTURE.md`
§2 lets name a concrete driver. For Oracle that is `crates/ffi` (M2.11). The binding holds **no
vendor syntax of its own**: it maps options onto the driver's `EXT_*` constants and, for a SID,
calls the driver's own builder, `reldex_driver_oracle_thin::sid_endpoint(host, port, sid, tls)`.
That function lives next to the driver's Easy Connect builder (`crates/drivers/oracle-thin/src/
endpoint.rs`) and shares its plain-name rule — a host or SID that could close a parenthesis or start
a keyword is refused, not escaped — and its `PROTOCOL` follows TLS (the driver refuses a TLS
profile whose descriptor says TCP). The Easy Connect path now calls the same module, byte-identical
to before (tested). A live test, `m2_9_sid_endpoint.rs`, proves the descriptor reaches the
listener: a wrong password through it gets `ORA-01017` (the SID resolved to an instance), an
unknown SID is a `Connection` error. Until M2.11 the reference binding lives in
`crates/workspace/tests/support/oracle_binding.rs` (the driver is a dev-dependency), so a renamed
key breaks a test and M2.11 can lift it unchanged; the crate's own unit tests use a neutral
`fake://` binding, so no Oracle syntax lives in `crates/workspace`. `connection_params` validates
the profile again (so the credential guard applies to the pure mapping too) and refuses a binding
for another database type, a missing password (the caller prompts), and a password offered to an
externally authenticated profile.

### P5 — The SQLite store

**One file**, `reldex.sqlite3`, in `com.reldex.reldex` under the per-user data directory:
`%LOCALAPPDATA%` on Windows (local, not roaming: the file will hold history, and the credentials it
refers to are per machine), `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` (absolute
only) or `~/.local/share` elsewhere on Unix. Android and iOS derive nothing: their platform layer
passes a path. Written here (~40 lines of environment lookups) because neither `dirs` nor
`directories` is in the dependency graph. Only `LOCALAPPDATA`, `HOME` and `XDG_DATA_HOME` are read,
so they are also how a user or a test points Reldex elsewhere (crate docs). A data directory, not a
config one, by choice: the file is application data, not a config file a user edits by hand; its
`-wal`/`-shm` companions must never roam or sync mid-write; and a later history file may move to
`XDG_STATE_HOME` if it is split out.

**Permissions.** `Store::open_default` creates the directory `0700` and a new store file `0600` on
Unix, before SQLite touches it (SQLite gives `-wal`/`-shm` the file's mode); existing ones are left
as they are. Windows is unchanged — `%LOCALAPPDATA%` is already per user.

**Schema 1** — two `STRICT` tables:

- `profile` — one column per `ProfileDetails` field (including `treat_as_production`, 0/1) plus
  `created_at`/`modified_at` (Unix ms). `password_in_credential_store` (0/1) is the only
  password-related column. Schema 1 has never shipped, so the column was added to it rather than
  as a step 2.
- `setting(scope, scope_id, setting_key, kind, int_value, text_value, updated_at)`,
  `PRIMARY KEY (scope, scope_id, setting_key)`, `WITHOUT ROWID`. `scope` is
  `application`/`profile`/`worksheet`, `scope_id` the profile or worksheet UUID (empty for
  application). `kind` names the value kind; `int_value` holds it, `NULL` meaning "no limit" /
  "unlimited"; `text_value` is reserved for enumerated kinds and unused by schema 1.
- Ids (`profile.id`, `setting.scope_id`) compare `COLLATE NOCASE`: the crate writes lowercase, but a
  row edited by hand in uppercase still names the same profile, so an update, a delete or a
  profile-scope write finds it rather than silently doing nothing (tested).

Enumerations are stored as fixed lowercase words and validated in code; `CHECK` constraints are
kept only for invariants that will never change, because SQLite cannot alter a `CHECK` and a new
database type must not need a table rebuild.

**Identity and version live in the header**: `PRAGMA application_id` = `0x524C4458` ("RLDX") and
`PRAGMA user_version` = schema version, chosen over a `schema_version` table because both are
written in the same transaction as the DDL they describe, can be read before any table exists,
and cannot disagree with the schema after a crash. An empty file (id 0, version 0, no objects) is
migrated from 0; a Reldex file newer than this build is refused with
`StoreError::NewerSchema { found, supported }`; any other file — including an SQLite database
that is someone else's — is refused with `StoreError::NotAReldexStore`; and a file with Reldex's
identity, version 0 and tables already in it — a header Reldex never writes — is refused with
`StoreError::IncompleteHeader`. Refusal happens **before anything is written**, including the WAL
switch, so an unfamiliar file is left byte-for-byte as it was (tested for each). The header is read in a single statement: read as three statements, identity,
version and table count could straddle another process's migration commit, and a store being
created next door was refused as foreign — found by looping the race test, fixed, and re-looped.

**Migration policy.** Forward only; `MIGRATIONS` is a contiguous list of steps, each a function
over one `IMMEDIATE` transaction; every pending step and the new header values commit together or
not at all (a failing step leaves the file exactly as it was — tested). A current file is only
*read* on open: no write lock is taken, so a second window never waits on the first. There is no
downgrade path. A setting key this build does not know (a newer Reldex's) is reported and **kept**,
never deleted. Query history (M4.10) and workspace (M6.2) arrive as new steps.

**Durability and concurrency.** WAL mode, `synchronous = FULL`, a busy timeout (default 5 s,
`StoreOptions`), every write one `IMMEDIATE` transaction so its check-then-write (does this profile
exist?) cannot race. Converting a new file to WAL is the one step of an open for which SQLite does
not consult the busy handler — two opens converting one new file at once got `SQLITE_BUSY`
immediately (found by the race test) — so the store retries that step with a short backoff for up
to the busy timeout. The timeout bounds each wait, not an open: one open can chain up to **four**
waits — the header read, the WAL conversion loop, the header re-read before migrating, and the
migration's `IMMEDIATE` lock — and on Windows each was measured at up to ~1.5× the timeout
(SQLite's busy handler sleeps in coarse steps there). There is no overall deadline; this is stated
on `Store::open*` and in the module docs so the service thread does not assume one.

**Errors, never panics.** `StoreError` is typed: `NewerSchema`, `NotAReldexStore`,
`IncompleteHeader`, `SchemaMismatch` (a statement SQLite could not prepare against the file's
tables — a file altered outside Reldex; SQLite's message names the SQL construct, never a value),
`Corrupt` (not a database, or damaged pages), `ReadOnly` (`SQLITE_READONLY`: a read-only file or
medium; reads work, the first write is refused and nothing is written), `Busy` (lock held past the
timeout; nothing written), `CannotOpen`, `NoDataDirectory`, `CreateDirectory`, `InvalidProfile`,
`InvalidSetting`, `ProfileNotFound`, `ProfileExists`, `InvalidRow`, and `Sqlite { code, detail }`
for the rest. `rusqlite`'s own error
type does not leave the crate. A stored row that no longer decodes or no longer passes its
descriptor is left out and reported in `Loaded::rejected`, and the level below is used — one bad
row never hides every profile.

### P6 — Threading contract

`Store` is `Send`, not `Sync` (checked at compile time, and by a `compile_fail` doctest). It is owned by the workspace service's own
thread — the thread that answers the UI's profile and settings requests — and is **never opened or
used on the UI thread**: every call is disk I/O and a write can wait for the busy timeout. The UI
asks that thread and is answered asynchronously, as for database work (`ARCHITECTURE.md` §6). It is
never on a session's worker thread either. Two handles on one file (two processes, a second
window) are safe: WAL readers never block the writer, and a writer that finds the lock held waits
and then succeeds, or fails with `Busy` — never a panic and never a partial write. Nothing in the
crate starts a thread. The service thread itself is M2.11/M3 work.

### P7 — No secret is ever written to SQLite

Enforced by construction, then proved:

1. **By type.** No type the store writes can hold a password: `Authentication::Password` has a
   user name and a `PasswordStorage` flag, nothing else. The password goes from the credential store
   (M2.10) straight into `connection_params` as a `Secret` (`db-driver-api`, redacting `Debug`, no
   `Display`) and never passes through the store.
2. **By schema.** No column can hold one; `the_schema_has_no_column_that_could_hold_a_secret` scans
   every column of every table for `password`, `passwd`, `pwd`, `secret`, `token`,
   `private_key` and `credential` (the one allowed match is the `password_in_credential_store`
   flag).
3. **On the bytes of a real file.** `tests/no_secrets.rs` puts a marker password into a stub
   credential store keyed by the profile's `CredentialKey`, runs every store write (insert, update,
   settings at all three levels, a clear, re-reads), fetches the password back through the stub,
   builds `ConnectionParams` through the Oracle reference binding and asserts the marker *is* the
   connection's password — then searches the file, its `-wal`, `-shm` and `-journal`, while open
   and again after close, in UTF-8, UTF-16LE and UTF-16BE. A positive control (a marker in the
   profile's name, which must be stored) proves the search finds text that is there.
4. **In `Debug`.** No type in the crate bears a secret; the tests assert that `Debug` of the
   profile, the store and the resulting `ConnectionParams` does not contain the marker. A connect
   string's `Debug` prints only its length, in `ProfileEndpoint` and in the stored row alike.
5. **In the endpoint's free text** — the rule below.

**The M2.10 seam** is `CredentialKey` (= the profile's id) and `Profile::credential_key()`; the
`CredentialStore` trait itself belongs to M2.10's own crate (its row says so), not here.
`delete_profile` deletes the profile's setting overrides in the same transaction; deleting the
password from the credential store is the caller's job, named on the method.

**Rule: credential-looking text in an endpoint is refused.** The host, service name, SID and
connect string are the one place a user could still paste a password (a connect string copied from
another tool with the logon in front). `ProfileDetails::validate` — which `Profile::create`,
`Profile::update`, the store's writes, the store's decoding of a row and `connection_params` all
run — refuses it with `ProfileError::CredentialInEndpoint { field, pattern: CredentialPattern }`,
which names the field and the pattern class and **never the text**. Matching is case-insensitive
and ignores all whitespace:

| `CredentialPattern` | Matches |
| --- | --- |
| `WalletPassword` | `wallet_password=` |
| `QueryParameter` | `?password=` or `&password=` |
| `PasswordKeyword` | any other `password=`, e.g. `(PASSWORD=…)` in a descriptor |
| `UserPasswordPrefix` | a `user/password@` prefix: `^[^/@()]+/[^@]+@` |

The rule is vendor-neutral — a guard against credential-looking text, not a parser of anyone's
syntax — and errs towards refusing; an endpoint that trips it can always be written without the
pattern. Near-misses stay accepted (a service named `PASSWORDS`, a host `pw.example`, an `@` inside
a parenthesised certificate DN — all tested). A refused save leaves no trace in the file (scanned).
A driver cannot add patterns yet: validation runs before a binding is chosen; a
`DriverBinding` hook can be added when a driver needs one. Hand-offs: the connection manager
(M3.2) must show this error without echoing the field; M3.7 owns two echoes outside this crate —
`ConnectionParams`' derived `Debug` prints a connect string verbatim (`db-driver-api`
`params.rs:228`), and the upstream `oracledb` parser echoes one in
`invalid connect string: {connect_string}: {reason}` (its `error.rs`).

### P8 — Dependencies added

Production dependencies of `reldex-workspace` (`cargo tree -p reldex-workspace -e normal,build
--prefix none`, identical on `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu` and
`aarch64-apple-darwin` apart from `libc` on the Unix targets):

| Crate | Version | Licence | New to the graph? | Why |
| --- | --- | --- | --- | --- |
| `rusqlite` | 0.40.2 | MIT | yes | owner decision §C.3 item 8; `default-features = false` (drops the statement cache and its `hashlink` dependency) |
| `libsqlite3-sys` (`bundled`) | 0.38.2 | MIT; bundled SQLite 3.53.2 is public domain | yes | compiles SQLite into the binary: Windows has no system SQLite, and a system copy's version and compile options would vary per machine |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 | yes | `rusqlite` |
| `fallible-streaming-iterator` | 0.1.9 | MIT/Apache-2.0 | yes | `rusqlite` |
| `smallvec` | 1.16.1 | MIT OR Apache-2.0 | yes | `rusqlite` |
| `vcpkg` (build) | 0.2.15 | MIT/Apache-2.0 | yes | `libsqlite3-sys` build script (unused with `bundled`) |
| `uuid` (`v4`) | 1.26.1 | Apache-2.0 OR MIT | no (via `oracledb`) | random profile and worksheet ids |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | no | `uuid` v4 |
| `bitflags`, `cfg-if`, `libc`; build: `cc`, `find-msvc-tools`, `shlex`, `pkg-config` | — | MIT OR Apache-2.0 | no | `rusqlite` / `libsqlite3-sys` |
| `reldex-db-driver-api` | — | GPL-3.0-or-later | — | the contract types the mapping produces |

`bundled` needs a C compiler at build time, which the workspace already requires for `aws-lc-rs`.
Dev-dependency only: `reldex-driver-oracle-thin` (for the reference binding's tests; never reaches
the product). No `dirs`, `directories` or `tempfile`: the test temp directory is a few lines of
`std`.

## Consequences

- M3.2/M3.6 get a complete model: every field `SPEC.md` §17 names, every registered default
  reachable with its levels, bounds and provenance, and a typed "no limit" consequence for M4.6's
  honest wording.
- M2.10 implements the credential store keyed by `CredentialKey`; nothing here changes.
- **M2.11 must** lift `tests/support/oracle_binding.rs` into `crates/ffi` behind its Oracle driver
  feature — it maps extension keys only and wires the driver's `sid_endpoint` — run the `Store` on
  the workspace service thread, carry `ProfileId` as 16 bytes, and name settings by numeric id.
- **M3.4** shows the production indicator from `Profile::treat_as_production()`, not from the
  environment enum.
- M4.10 and M6.2 add tables as migration steps 2 and 3. M6.2 also decides when a closed
  worksheet's overrides are deleted (`clear_worksheet_settings`).
- **Accepted limitations** (review of 2026-09-25, one line each):
  - The WAL-conversion retry sleeps on the calling thread — the workspace service thread, never
    the UI thread — for at most the busy timeout.
  - A settings screen that applies several changes issues one transaction per setting; values are
    validated before any write, so only an I/O failure could apply part of a batch (a batch write
    can be added if M3.6 needs it).
  - Worksheet overrides are keyed by worksheet id alone, with no worksheet table to reference,
    until M6.2 adds one.
  - `results.fetches_in_flight` is application-only (P2).
  - An open has no overall deadline, only a per-wait one (P5).
- `crates/workspace` is not in the mobile cross-compile workflow's package set yet: the bundled
  SQLite build for Android/iOS is unverified (Phase 4/5).
- Not decided here and deliberately not built: display settings (M5), the metadata cache (P2), the
  entitlement feature service (`SPEC.md` §22, P6) — nothing here is an `is_pro` check or blocks one.

## Alternatives considered

- **A `db-core` module**, as the plan row sketched — rejected for P1's reasons.
- **A `schema_version` table** instead of the header — rejected: it can disagree with the schema
  after a hand edit or a crash between two statements, cannot be read before it exists, and does not
  mark the file as Reldex's. The header is atomic with the DDL and gives identity for free.
- **One JSON/TOML settings blob** — rejected: no per-value provenance timestamp, no partial read when
  one value is bad, and hand-editability is a trade-off the owner already accepted losing
  (§C.3 item 8).
- **One column per setting** — rejected: every new setting would be a migration and a table rebuild
  for any change of type.
- **Oracle extension keys and descriptor syntax inside this crate**, or a dependency on
  `reldex-driver-oracle-thin` — rejected: vendor behaviour in a vendor-neutral crate, or a
  core-to-driver edge `ARCHITECTURE.md` §2 forbids. The `DriverBinding` seam costs ~60 lines at the
  composition root.
- **A `CredentialStore` trait here** — rejected: M2.10's row puts it in `crates/secrets`, and nothing
  in this crate calls it.
- **System SQLite** — rejected: absent on Windows, version- and option-variable elsewhere.
- **`dirs`/`directories`** — rejected: not in the graph, and three documented environment lookups
  do not justify a dependency.
- **`sqlx`/`diesel`** — rejected: async runtimes or code generation for a dozen statements.

## Evidence

`cargo test -p reldex-workspace`: 100 tests plus 2 doctests on Windows (101 on Unix, where the
permissions test runs), no database, no network. `reldex-driver-oracle-thin`: 4 unit tests of the
endpoint builders, a doctest, and the live `m2_9_sid_endpoint.rs` (2 tests, passed against the
19c test container).

- Resolution: `truth_table_for_a_setting_allowed_at_every_level` and
  `truth_table_for_a_setting_that_skips_the_worksheet_level` — each level in one of four states (no
  layer, layer without the value, a value unique to the level, the built-in default's value), all
  4³ = 64 combinations per setting, disallowed values forced in to prove resolution skips them,
  checked against the rule written out independently of the resolver.
- Registry: every owner default and its levels asserted one by one; every default satisfies its
  own descriptor; storage keys unique and stable; typed handles checked at compile time.
- Store: every profile shape (all six environments, service/SID/descriptor, every role, TLS
  options, external and prompt-each-time authentication, Thai text) round-trips; every setting kind
  round-trips at every level it allows and is refused at every level it does not; rejected rows are
  reported, kept, and the level below used.
- Files (`tests/store_file.rs`): new file in WAL mode with the Reldex identity; a v1 file reopens
  byte-identical; a newer-version file, a foreign SQLite file, an identified file with tables but
  no version and a non-database file are refused with typed errors and left byte-identical (no
  `-wal`/`-shm` created); tables that differ from their version give `SchemaMismatch`; a read-only
  file reads and refuses the first write with `ReadOnly`, unchanged; damaged pages give `Corrupt`,
  not a panic; a missing directory gives `CannotOpen`. In-crate: an uppercase id round-trips
  through read, update, profile-scope write and delete; on Unix the created directory is `0700`
  and the file and its WAL `0600`.
- Concurrency (`tests/two_handles.rs`): a reader is not blocked by a held write lock and sees only
  committed data; a writer with no patience gets `Busy` and writes nothing; a patient writer waits
  and succeeds; an exclusively locked file is `Busy` at open; four opens racing on a new file all
  succeed and exactly one migrates. Looped 200 times sequentially and 100 times as two concurrent
  processes, clean — after the two defects the loop found (P5) were fixed.
- Mapping (`src/connect.rs`, `tests/connect_oracle_binding.rs`): service, SID (the driver's own
  descriptor, TLS following the transport) and connect-string endpoints; TLS options; "no connect
  limit" and "rewrite off" through the driver's own keys; a SID or host that could rewrite the
  descriptor is refused; and, against the real
  `OracleThinDriver::connect` with no socket opened, an unenforceable `SSL_SERVER_CERT_DN` pin is
  refused without the opt-out and a TLS profile with a plaintext descriptor is refused.
- No secrets: P7 — plus each credential pattern refused (in the connect string, host and service
  name) with a value-free error, the near-misses accepted, and a file scan after refused saves.
- Dependency rule (`tests/dependency_rules.rs`): the crate's normal dependencies are exactly
  `reldex-db-driver-api`, `rusqlite` and `uuid`; `db-core`, the driver contract and every driver
  depend on neither this crate nor `rusqlite`.
