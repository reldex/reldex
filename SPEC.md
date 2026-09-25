# Reldex — Product & Technical Specification

**Status:** Initial architecture specification — amended 2026-09-19 after Phase 0 spikes (owner-approved)  
**Product:** Reldex  
**Category:** High-performance cross-platform database development environment  
**Primary platform:** Desktop  
**Secondary platforms:** Tablet and mobile  
**Core:** Rust  
**UI:** Qt Quick / QML  
**Initial compatibility:** Oracle Database 19c+  
**Business model:** Community / support & tips / optional Pro

## 1. Vision

Reldex is a modern database development environment for developers, database developers, DBAs, and advanced technical users.

Reldex is desktop-first. Tablet and mobile are intended to use the same underlying database core and, where technically viable, connect directly to supported databases rather than requiring a middleware gateway.

The product should compete on:

- interaction latency
- smooth native-feeling UI
- startup time
- memory efficiency
- correct stateful database sessions
- correct transaction semantics
- scalable large-result handling
- modern developer workflow
- cross-platform architecture
- extensibility

## 2. Engineering priorities

When requirements conflict, use this order:

1. Database correctness
2. Session and transaction correctness
3. UI responsiveness
4. Reliability
5. Runtime performance
6. Memory efficiency
7. Cross-platform behavior
8. Maintainability
9. Developer convenience

Never trade transaction correctness for implementation simplicity.

## 3. Branding

The product brand is **Reldex**.

Vendor names must not be incorporated into the product name, logo, domain, executable branding, app-store title, or primary product identity.

Vendor names may be used descriptively where required for compatibility, drivers, documentation, and error messages.

## 4. Platforms

### Tier 1
- Windows
- macOS
- Linux

Windows is the first release target.

### Tier 2
- iPadOS
- Android tablets

### Tier 3
- Android phones
- iPhone

Mobile is not defined as monitoring-only. Feature differences should result from screen size, platform lifecycle, or technical limitations rather than an intentionally reduced database core.

## 5. High-level architecture

```text
                  Database Server
                        ▲
                        │
                 Database Protocol
                        │
                  Database Driver
                        │
                ┌───────┴────────┐
                │   Rust Core    │
                │                │
                │ Sessions       │
                │ Transactions   │
                │ Query Engine   │
                │ Result Engine  │
                │ Metadata       │
                │ Workspace      │
                └───────┬────────┘
                        │
                   Stable FFI
                        │
                C++ / Qt Adapter
                        │
                 Qt Quick / QML
                        │
          ┌─────────────┼─────────────┐
       Desktop        Tablet         Phone
```

The Rust core must be UI-independent.

## 6. Core modules

Use vendor-neutral names:

```text
DatabaseDriver
DatabaseConnection
DatabaseSession
QueryExecutor
TransactionManager
ResultStore
MetadataProvider
WorkspaceService
```

Vendor-specific types belong only in driver/provider implementations.

## 7. Driver strategy

The default connection path should use a pure/thin implementation that does not require Oracle Instant Client or OCI libraries.

Initial compatibility target:

```text
Oracle Database 19c+
```

The concrete Rust driver must be encapsulated behind the internal driver API because its maturity and platform support may change.

Desktop may later offer an optional native/OCI-based compatibility driver. Mobile must never require OCI.

## 8. Mandatory Phase 0 gate

Before building the full GUI, prove the database architecture with `reldex-core-poc`.

Required validation:

- Windows x64
- Linux x64
- macOS ARM64
- Android ARM64 physical device
- iOS/iPadOS ARM64 physical device

Mobile direct-connect must not be advertised until real-device testing succeeds.

### Driver test matrix

Validated against Oracle's official `oracledb` crate v26.0.0-beta.3 (ADR-0001), with exceptions
recorded in `docs/exec-plans/active/phase-0.md` (Workstreams C–G) and
`docs/exec-plans/active/phase-0-spike-results.md`: cancellation does not meet §10/§24.8 (see the
interim note under §10); TCPS is validated only as one-way TLS 1.2 with an AEAD suite, trusting a
PEM the user supplies — mutual TLS combined with a private CA, reading an existing Oracle wallet
(`ewallet.p12`/`cwallet.sso`), OS trust stores and certificate revocation are not available with this
driver version (spike S8, U-12…U-14). Network loss/reconnect, NCLOB, EXPLAIN PLAN/DBMS_XPLAN,
metadata/dictionary access and privileged connections are all validated (spikes S10–S14): a dead
socket is detected and reported honestly (`NetworkLost`/`Lost`) in microseconds, and nothing
reconnects silently — but a **black-holed** connection needs a caller-set deadline to return at all,
and that deadline then costs the session (no TCP keepalive exists upstream and `EXPIRE_TIME` is parsed
and never used — U-16, U-17). A **connect** cannot be bounded by the upstream crate either (U-15), so
the driver bounds it itself — see "Connect timeout" below; nothing can interrupt an abandoned attempt,
which costs one thread until it finishes. `CREATE TRIGGER` with `:NEW`/`:OLD` cannot be executed by the
upstream crate directly, because its parser treats them as bind placeholders even inside DDL (U-18); the
driver applies the `BEGIN EXECUTE IMMEDIATE q'[…]'; END;` workaround automatically — see "Trigger DDL
auto-rewrite" below — subject to a 32767-byte trigger-text limit and with any syntax error's position
referring to the wrapper block. Mobile: cross-compile to
`aarch64-linux-android`/`aarch64-apple-ios`/`aarch64-apple-ios-sim` is proven in CI (spike S6), and the
core + driver + TLS stack has run on a physical Android arm64 phone as a native binary
(`phase-0-android-device.md`); the packaged-app path and any iOS device evidence are still pending
(§25, unchanged).

Connectivity:
- host/port
- service name
- Easy Connect
- connect descriptor
- TCP
- TCPS
- username/password
- privileged connections where supported

**Connect timeout (owner decision 2026-09-19).** The Oracle driver must honour a caller-set
connect timeout (`ConnectionParams::connect_timeout`; see `phase-0-spike-results.md` §7 C-5 and
U-15) itself, by running the upstream connect on a helper thread and giving up once the limit
passes — an abandoned attempt is left to finish or fail on its own, and no session is ever adopted
after the limit. Default **15 seconds**; user-configurable per connection profile (§17), including
"no limit". This is driver-specific behavior; the core connection contract is unchanged.

**Trigger DDL auto-rewrite (owner decision 2026-09-19).** For a `CREATE TRIGGER` body containing
`:NEW`/`:OLD` (U-18), the Oracle driver automatically rewrites the DDL into
`BEGIN EXECUTE IMMEDIATE q'[…]'; END;` (choosing a quote delimiter that cannot collide with the
body). This rewrite is **on by default**, is always reported to the user as a warning on the
outcome with the statement actually sent available for inspection, and can be turned **off** per
connection, in which case the existing explanatory refusal (§8 above) is returned instead. This is
driver-specific (vendor) behavior; the core DDL-execution path stays vendor-neutral.

SQL:
- SELECT
- INSERT
- UPDATE
- DELETE
- MERGE
- DDL

Procedural SQL:
- anonymous blocks
- procedure/function
- package
- IN/OUT/IN OUT binds
- REF CURSOR

Types:
- NUMBER
- CHAR/VARCHAR2/NVARCHAR2
- DATE
- TIMESTAMP
- TIMESTAMP WITH TIME ZONE
- RAW
- CLOB/NCLOB/BLOB
- JSON where applicable

Type caveats (Phase 0, `oracledb` 26.0.0-beta.3): a named-region `TIMESTAMP WITH TIME ZONE` value is
refused at describe time — containment, not support, see the §10 interim note; a genuine `JSON`
column type (21c+), `XMLType`, `VECTOR`, object types and `BFILE` are refused at describe time for
the same driver-version reason. Oracle 19c's own JSON-via-VARCHAR2/CLOB/BLOB is unaffected. Evidence:
`phase-0-spike-results.md` §3 and §5 (U-3).

Transactions:
- COMMIT
- ROLLBACK
- SAVEPOINT
- ROLLBACK TO SAVEPOINT

Operations:
- long query
- cancellation
- timeout
- network loss
- reconnect
- concurrent sessions
- large result
- large LOB
- DBMS_OUTPUT
- metadata/dictionary access
- EXPLAIN PLAN / DBMS_XPLAN

## 9. Session model

A worksheet owns a stable database session.

```text
Connection Profile
 ├─ Worksheet A → Session A
 ├─ Worksheet B → Session B
 ├─ Object Browser → Metadata Session
 └─ Monitor → Monitoring Session
```

A worksheet session may hold:

- an active transaction
- current schema
- ALTER SESSION state
- NLS configuration
- package state
- temporary-table state
- DBMS_OUTPUT state
- application context

A generic web-style connection pool must not arbitrarily replace a worksheet session.

## 10. Transaction model

Default:

```text
Auto-commit = OFF
```

Every worksheet must expose Execute, Cancel, Commit, and Rollback. On-demand Cancel remains the
product requirement — Reldex competes on correct cancellation.

> **Interim limitation (owner decision, 2026-09-19).** With the current primary driver version
> (`oracledb` 26.0.0-beta.3, [ADR-0001](docs/decisions/0001-database-driver-strategy.md)), on-demand
> Cancel is not available: no mechanism stops a running statement and keeps the session usable in the
> general case. Until the driver exposes a break/cancel mechanism, Reldex offers a per-statement time
> limit set before execution instead. The UI must never present this limit as Cancel, and must tell
> the user plainly that on-demand Cancel is unavailable with the current driver. See §24.8.

**Per-statement time limit defaults and configuration (owner decision 2026-09-19).** Reldex arms
a default per-statement time limit on worksheet statements; the initial default is **600 seconds**,
to be revisited with real usage in Phase 1. It must be user-configurable at three levels —
application default, connection profile, and per worksheet/statement — including "no limit". When
the user selects "no limit", the UI must plainly explain the consequence: a hung statement can then
only be abandoned by closing the session. This does not change the constraints in the interim note
above: the limit must never be presented as Cancel, the UI states before running that a limit
applies, and when the limit fires the session may be lost (see `phase-0-spike-results.md` U-6) and
the UI must say so honestly.

Closing a worksheet with an active transaction must ask the user to Commit, Rollback, or Cancel closing. Never silently commit.

## 11. UI architecture

Qt Quick/QML is the presentation layer.

The adapter path is:

```text
QML
 ↓
QObject / QAbstractItemModel
 ↓
Thin C++ Adapter
 ↓
Stable Rust FFI
 ↓
Reldex Core
```

The C++ adapter contains integration code, not business rules.

No database or network I/O may occur on the UI thread.

## 12. Result architecture

Never create one QML object per database row for large results.

Required flow:

```text
Database Cursor
      ↓
Batch Fetch
      ↓
Result Store
      ↓
Virtual Table Model
      ↓
Visible Rows/Cells
      ↓
Qt Quick TableView
```

The result store should support:

- typed values
- NULL representation
- batches
- bounded memory
- lazy large-value access
- streaming export
- efficient random access

Apache Arrow may be used internally where benchmarks show a benefit. UI APIs must not depend directly on Arrow.

**Fetch batch size (owner decision 2026-09-19).** No default fetch batch size is fixed yet:
`phase-0-spike-results.md` S14 found throughput is **not** monotonic in batch size (10,000
rows/batch measured 3.5× slower than the best of 100 and 1,000, with the longest wait for the
first row), so the shipped default will be set from a Phase 1 benchmark across row shapes and a
real network. Until then the driver's current default is used. Fetch batch size must be a user
setting — an application default and a per-connection-profile override — within a bounded range.

## 13. Result grid

V1:
- row/cell virtualization
- column resize/reorder
- sorting/filtering
- row numbers
- NULL visualization
- copy cell/row/range
- copy with headers
- search
- type-aware formatting
- CLOB/BLOB viewers

Future:
- pinned columns
- grouping/aggregation
- editable results
- conditional formatting
- pivoting

## 14. SQL / PL/SQL editor

V1:
- syntax highlighting
- line numbers
- folding
- search/replace
- bracket matching
- indentation
- large-file handling
- Unicode/Thai/IME
- high-DPI
- light/dark themes
- configurable fonts

Architecture must allow future autocomplete, signatures, diagnostics, go-to-definition, find references, and dependency navigation.

## 15. Script execution

Do not split scripts by every semicolon.

The parser must understand SQL and PL/SQL block boundaries and `/`.

Future SQL*Plus-like commands may be added progressively.

## 16. Object browser and metadata

Initial object groups:

- Schemas
- Tables
- Views
- Packages
- Package Bodies
- Procedures
- Functions
- Triggers
- Sequences
- Synonyms

All metadata loading must be lazy.

Metadata access goes through a generic `MetadataProvider`. Vendor dictionary SQL stays in the vendor provider.

A local SQLite metadata cache may be used with TTL, refresh, invalidation, and per-database identity isolation.

## 17. Connection profiles and credentials

Profiles include:

- database type
- environment
- host/port
- service/SID/descriptor where applicable
- authentication
- session options
- display options

Environments:

- Development
- Test
- UAT
- Staging
- Production
- Custom

Production should have a persistent visual indicator.

Passwords must use platform secure storage where available:
- Windows Credential Manager
- Apple Keychain
- Android Keystore
- Linux Secret Service

Where no secure store is available, the password is asked for at every connect. There is never a
plaintext fallback (owner decision 2026-09-20; [ADR-0007](docs/decisions/0007-credential-store.md)).

## 18. Mobile

Mobile connects directly when the driver/platform supports it.

A gateway may be implemented later as an optional connection mode.

On resume:
1. validate the existing session;
2. continue only if it is still alive;
3. otherwise mark it disconnected;
4. never silently replace a lost transactional session.

A reconnect creates a new database session.

## 19. Performance objectives

These are targets, not guarantees.

- normal interaction: target 60 FPS minimum
- 120 FPS desirable on capable systems
- no database work on UI thread
- virtualized result handling for 1,000,000+ logical rows
- metadata browsing must remain usable with 100,000+ objects
- warm desktop startup <1 second desirable; <2 seconds acceptable

Track:
- frame time
- CPU
- RAM
- allocations
- startup time
- fetch throughput
- render latency
- metadata latency
- cancellation latency
- export throughput

Profile before optimizing.

## 20. Local persistence

SQLite may store:

- profiles without plaintext secrets
- workspace
- query history
- metadata cache
- favorites
- snippets
- settings
- UI layout
- feature state

## 21. Export

V1:
- CSV
- TSV
- JSON
- SQL INSERT
- clipboard

Future:
- XLSX
- Parquet
- Arrow IPC

Large export must stream instead of loading the complete result into memory.

## 22. Editions

### Community
Expected baseline:
- connection manager
- SQL editor
- PL/SQL editor
- result grid
- object browser
- DBMS_OUTPUT
- explain plan
- basic export

Licence: GPL-3.0-or-later (owner decision 2026-09-24); Pro is licensed separately by the copyright
holder; external contributions require a CLA so dual licensing stays possible.

### Pro
Candidates:
- schema compare
- data compare
- advanced monitoring
- performance analysis
- automation
- advanced export
- AI assistant
- gateway/team features

Entitlements must use a centralized feature service rather than scattered `is_pro` checks.

### Third-party notices

Any distributed build — Community or Pro — must ship a generated third-party notices file (e.g. via
`cargo about`) covering the full transitive dependency graph, not just direct dependencies, before
first binary distribution. The oracle-thin driver alone carries 55 third-party crates at run time (63
including build-only crates), all permissively licensed but requiring attribution.

## 23. Non-goals for V1

Do not delay V1 for:
- full SQL*Plus compatibility
- PL/SQL debugger
- ER modeling suite
- migration suite
- plugin marketplace
- AI
- cloud sync
- gateway
- team collaboration
- multiple database vendors

## 24. Definition of Done — Desktop V1

A developer can:

1. install Reldex;
2. create connection profiles;
3. connect to Oracle Database 19c through the thin path;
4. open independent worksheet sessions;
5. execute SQL and PL/SQL;
6. use bind variables;
7. browse large results without freezing the UI;
8. cancel running statements — **not yet met — blocked on upstream driver (ADR-0001)** (see the §10 interim note);
9. commit and roll back explicitly;
10. use DBMS_OUTPUT;
11. browse database objects;
12. inspect tables;
13. open and compile PL/SQL source;
14. view compile errors — token-level error-position highlighting is scoped to PL/SQL compilation errors; ordinary SQL errors report the native ORA-nnnnn code and message without a character position (upstream limitation, see ADR-0002 "Notes for driver implementers");
15. run explain plan;
16. save and restore non-transactional workspace state;
17. continue interacting with the application while other sessions execute.

## 25. Definition of Done — Mobile support

A mobile platform is supported only after a physical device can:

- connect directly
- execute SQL and PL/SQL
- bind values
- use REF CURSOR
- work with LOBs
- commit/rollback
- cancel
- use TCPS
- survive background/resume correctly
- reconnect without hiding session loss

## 26. First implementation

The first implementation is `reldex-core-poc`, not the full editor UI.

Only after Phase 0 passes should the main Qt Quick desktop UI be treated as the primary development stream.
