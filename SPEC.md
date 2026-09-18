# Reldex — Product & Technical Specification

**Status:** Initial architecture specification  
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

Connectivity:
- host/port
- service name
- Easy Connect
- connect descriptor
- TCP
- TCPS
- username/password
- privileged connections where supported

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

Every worksheet must expose Execute, Cancel, Commit, and Rollback.

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
8. cancel running statements;
9. commit and roll back explicitly;
10. use DBMS_OUTPUT;
11. browse database objects;
12. inspect tables;
13. open and compile PL/SQL source;
14. view compile errors;
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
