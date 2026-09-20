# Reldex

**Reldex** is a high-performance, cross-platform database development environment designed for developers and DBAs.

The product is desktop-first, with tablet/mobile support planned around the same database core. Reldex is vendor-neutral at the product and core layers; database-specific support is implemented through drivers and providers.

Reldex is an independent project and is not affiliated with, endorsed by, or sponsored by Oracle Corporation. Oracle is a registered trademark of Oracle and/or its affiliates.

## Current status

**Phase 0 closed with a GO for Phase 1 from the owner on 2026-09-19.** The Phase 1 Desktop MVP plan
(milestones M1–M6) is in [`docs/exec-plans/active/phase-1.md`](docs/exec-plans/active/phase-1.md), and
[ADR-0003](docs/decisions/0003-qt-rust-integration.md) (Qt ↔ Rust integration) is drafted as
**Proposed**, pending spike S15. The owner approved the toolchain and the
Phase 1 kickoff decisions on 2026-09-20; the toolchain is installed (`docs/exec-plans/active/phase-1-toolchain.md`)
and spike S15 is next. The Phase 0 tail work is done: the driver now enforces a connect timeout (C-5),
rewrites `CREATE TRIGGER` DDL that upstream cannot execute (U-18) and reports connect-time warnings (C-6);
and the core + driver + TLS stack has run on a physical Android phone as a native binary
(`docs/exec-plans/active/phase-0-android-device.md`). Still open: the packaged-app path on Android and any iOS device evidence.

**Phase 0 — Architecture Validation.** Spikes S1–S5, S7, S8 and S9 ran against a live Oracle Database
19.3 (Docker) on 2026-09-19; the evidence-gap spikes S10–S14 (network loss/reconnect, NCLOB, developer
features, privileged connections, large result) ran against the same database on 2026-09-19; the
mobile cross-compile spike S6 ran in CI on 2026-09-19. Full evidence is in
[`docs/exec-plans/active/phase-0-spike-results.md`](docs/exec-plans/active/phase-0-spike-results.md)
and, for S6,
[`docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md`](docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md).

**What works against a real Oracle 19c today:** connect via Easy Connect or a full TNS descriptor,
including `AS SYSDBA`/`AS SYSOPER` privileged connections; SELECT/DML/DDL; PL/SQL blocks, procedures,
functions and packages with IN/OUT/IN OUT binds and DBMS_OUTPUT; REF CURSOR fetched to exhaustion;
COMMIT/ROLLBACK/SAVEPOINT with auto-commit off and no session-state leakage; CLOB/BLOB/NCLOB streamed
in bounded memory (100 MB CLOB/BLOB for +4.7 MB working set; NCLOB Thai/non-BMP text byte-exact); 1
million rows streamed for about 1 MB of working-set growth; NUMBER and character data (including Thai
and non-BMP text) read back exact; developer features — EXPLAIN PLAN, DBMS_XPLAN, `V$` views, ten
dictionary views and `LONG`/`LONG RAW` columns; network-loss detection (a dead socket is reported in
microseconds) and reconnect semantics (nothing silently reconnects or inherits old session state);
one-way TLS 1.2 (TCPS) with certificate and host-name verification, trusting a user-supplied CA; 8
concurrent sessions running independently. Mobile cross-compile (Android/iOS) is proven in CI.

**Known limitations, stated plainly:**

- **No on-demand statement cancel.** Only a pre-armed deadline exists, and it can destroy the session
  if the server does not interrupt promptly (spike S4 fails `SPEC.md` §10/§24.8's requirement). The
  owner decided on 2026-09-19 to accept this as a limitation — stay on `oracledb`, ship the pre-armed
  deadline with an honest UI, and pursue upstream fixes — rather than change drivers; see
  [ADR-0001](docs/decisions/0001-database-driver-strategy.md) "Owner decision (2026-09-19)".
- **`TIMESTAMP WITH TIME ZONE` columns are refused** at describe time by default, because a
  named-region value aborts the process in the upstream driver; this is containment, not support.
- **NUMBER bind restrictions**: certain decimal shapes (an odd count of leading zeros below 0.1, or a
  40-digit value at specific decimal-point positions) are refused on bind rather than corrupted or
  crashed.
- **A connect cannot be bounded, and there is no dead-link keepalive.** `ConnectionParams::connect_timeout`
  is accepted by the contract and ignored by the driver (a connect can take 22 s against an unroutable
  address, or hang indefinitely against a black hole); no TCP keepalive exists upstream and
  `SQLNET.EXPIRE_TIME` is parsed and never acted on, so a client that goes silent mid-transaction can
  hold a row lock indefinitely (spike S10, U-15…U-17).
- **`CREATE TRIGGER` with `:NEW`/`:OLD` cannot be executed directly** — the upstream parser treats them
  as bind placeholders even inside DDL (U-18). A documented `EXECUTE IMMEDIATE` workaround exists.
- **TCPS is narrower than most enterprise deployments expect**: no mutual TLS combined with a private
  CA, no Oracle wallet file support, no OS trust store, no certificate revocation, and
  `SSL_SERVER_DN_MATCH` is silently ignored upstream (spike S8, U-12…U-14).
- **Native Network Encryption and 11G password verifiers are unsupported** by the primary driver.
- **Mobile has no physical-device evidence yet.** Android/iOS/iOS-simulator cross-compile and link are
  proven in CI (spike S6, no local NDK/Xcode needed) — that is cross-compile evidence only, not mobile
  support. The blocker is that no physical test device has been provided; see
  [`phase-0-s6-mobile-cross-compile.md`](docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md)
  "Next step toward physical-device validation".

- [ADR-0001](docs/decisions/0001-database-driver-strategy.md) — **Accepted — owner decision
  2026-09-19: stay on oracledb; pre-armed deadline + honest UI; upstream issues #21–#25 filed
  2026-09-19**: the primary database driver remains Oracle's official `oracledb` crate
  (`oracle/rust-oracledb`); the owner accepted the cancellation limitation rather than changing
  drivers.
- [ADR-0002](docs/decisions/0002-driver-api-and-concurrency-model.md) (driver API and concurrency
  model) — **Accepted (owner confirmed 2026-09-19) — implemented; independently reviewed twice with
  must-fix findings applied; amended after the Phase 0 spikes**.
- A Cargo workspace with `crates/db-driver-api`, `crates/db-core` (session/worker-thread layer),
  `crates/drivers/mock`, `crates/drivers/oracle-thin` (wraps `oracledb`) and `crates/reldex-core-poc`
  all implemented, plus GitHub Actions CI (`cargo fmt`, `cargo clippy`, `cargo test` on
  Windows/Linux/macOS — this branch has not yet gone through a PR/CI run).
- A local Oracle 19c Docker test database under [`tools/oracle-test-db/`](tools/oracle-test-db/),
  verified (AL32UTF8, Thai round-trip, Non-CDB).

**Not started yet:** the Qt Quick/QML UI; driver-upgrade contract tests; honouring `connect_timeout`
(C-5); Android/iOS physical-device validation (cross-compile is done; a test device is not yet
available).

[`Task.html`](Task.html) (open it in a browser) is the live, human-facing progress dashboard: current focus, blockers/risks, spike results, and recent activity. [`TASKS.md`](TASKS.md) and the active plan under [`docs/exec-plans/active/phase-0.md`](docs/exec-plans/active/phase-0.md) remain the source of truth for task status; `Task.html` mirrors them.

## Technology decisions so far

- **Core:** Rust.
- **UI:** Qt Quick/QML with a thin C++ adapter over a stable Rust FFI boundary — planned, not started.
- **Primary database driver:** Oracle's official [`oracledb`](https://github.com/oracle/rust-oracledb) crate (pure Rust, thin, blocking; no Instant Client/OCI required), pinned to an exact pre-GA beta version (`=26.0.0-beta.3`), per [ADR-0001](docs/decisions/0001-database-driver-strategy.md). It is encapsulated behind `db-driver-api` so it can be swapped if a kill criterion in the ADR's spike plan fires. Spike S4's cancellation kill criterion fired, and on 2026-09-19 the owner decided to accept the limitation — ship the pre-armed deadline with an honest UI and pursue upstream fixes — rather than change drivers; see the current limitations above.
- **Known gaps in the primary driver**, stated honestly: no on-demand statement-cancel API (Phase 0 falls back to a pre-armed deadline; five of seven drafted upstream issues were submitted 2026-09-19, #21–#25; the owner decided 2026-09-19 not to submit F and G for now, keeping the drafts for tracking only); a connect cannot be bounded and there is no dead-link keepalive; `CREATE TRIGGER` with `:NEW`/`:OLD` cannot be executed directly (documented workaround); it is pre-GA/beta software with at least one defect that can abort the whole process if an unhandled input reaches it; Native Network Encryption and 11G password verifiers are unsupported; Android/iOS cross-compile is proven in CI but physical-device evidence is still required before any mobile-support claim.
- **Initial compatibility target:** Oracle Database 19c+.

## Running the integration suite

`cargo test --workspace` runs unit tests only and never touches a database. The database-backed
integration suite is opt-in (`--features oracle-it`) and requires the local Oracle 19c test database —
see [`tools/oracle-test-db/README.md`](tools/oracle-test-db/README.md) for setup and connection
details. Run the cancellation spike single-threaded; it includes long cartesian joins, a
`KILL SESSION` and a 20-second PL/SQL sleep whose outcome is load-dependent under parallel execution:

```sh
tools/oracle-test-db/run-it.ps1 s4_cancel -- --test-threads=1
tools/oracle-test-db/run-it.sh  s4_cancel -- --test-threads=1
```

## Product principles

1. Database and transaction correctness first
2. UI responsiveness and low latency
3. Native/GPU-driven presentation
4. Bounded memory usage
5. Large-result virtualization
6. Stateful worksheet sessions
7. Desktop-first, mobile-capable architecture
8. Vendor-neutral core
9. No database I/O on the UI thread
10. Benchmark before optimization

## Repository layout

```text
crates/
  db-driver-api/          vendor-neutral driver contract + DbError (implemented, ADR-0002)
  db-core/                sessions, transactions, query, results, metadata, workspace (implemented)
  drivers/mock/            test-support/mock driver for core tests (implemented)
  drivers/oracle-thin/     thin driver wrapping Oracle's `oracledb` crate, ADR-0001 (implemented; spikes S1-S5, S7, S9 run)
  reldex-core-poc/         Phase 0 validation harness binary (no full UI)
docs/
  architecture/            architecture source of truth (ARCHITECTURE.md)
  decisions/               ADRs (see docs/decisions/README.md)
  exec-plans/active/       current execution plan(s), e.g. phase-0.md
tools/
  oracle-test-db/          local Oracle 19c Docker test database for integration tests
.agents/skills/            repository-local skill shared by all coding agents (reldex-development)
.claude/skills/            Claude Code entry point; thin wrapper around .agents/skills
.github/workflows/         CI (fmt, clippy, test) and mobile-cross-compile.yml (Android/iOS build+link check, spike S6)
AGENTS.md                  repository rules for coding agents
CLAUDE.md                  Claude Code entry point; imports AGENTS.md
SPEC.md                    product and technical specification
ROADMAP.md                 development phases
TASKS.md                   current project task board (source of truth)
Task.html                  live progress dashboard (open in a browser)
```

## Getting started (developers)

Prerequisites:

- Rust (stable, via [`rust-toolchain.toml`](rust-toolchain.toml)) with `rustfmt` and `clippy`.
- Docker Desktop — only needed for database-backed integration tests, not for building or running unit tests.

CI (`.github/workflows/ci.yml`) runs, on Windows, Linux, and macOS:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`.github/workflows/mobile-cross-compile.yml` separately builds and links the core/driver crates for
`aarch64-linux-android`, `aarch64-apple-ios` and `aarch64-apple-ios-sim` on GitHub-hosted runners (no
local NDK/Xcode) — this is spike S6; see
[`phase-0-s6-mobile-cross-compile.md`](docs/exec-plans/active/phase-0-s6-mobile-cross-compile.md) for
what it does and does not prove.

Run the Phase 0 validation harness:

```sh
cargo run -p reldex-core-poc
```

`cargo test --workspace` runs unit tests only and never depends on a database, network access, or secrets. Database-backed integration tests are separate and require the local Oracle 19c test database under [`tools/oracle-test-db/`](tools/oracle-test-db/); see that directory's README for setup, verified connection details, and limitations.

## For coding agents

- `AGENTS.md` is the repository rule file (single source of truth); `CLAUDE.md` imports it.
- Use the `reldex-development` skill (`.agents/skills/reldex-development/SKILL.md`, wrapped for Claude Code at `.claude/skills/reldex-development/SKILL.md`) for any repository work.
- ADRs under `docs/decisions/` are required for changes to a core boundary, session/transaction semantics, driver strategy, Qt/Rust integration, result-storage architecture, or cross-platform support expectations.
- Keep [`Task.html`](Task.html) updated in the same change whenever task status, an ADR, a phase gate/spike result, or a blocker changes (`AGENTS.md`, "Progress dashboard").

## Documentation map

- [`SPEC.md`](SPEC.md) — product and technical specification
- [`docs/architecture/ARCHITECTURE.md`](docs/architecture/ARCHITECTURE.md) — architecture source of truth
- [`ROADMAP.md`](ROADMAP.md) — development phases
- [`TASKS.md`](TASKS.md) — current task board (source of truth for status)
- [`docs/exec-plans/active/phase-0.md`](docs/exec-plans/active/phase-0.md) — active execution plan
- [`docs/decisions/README.md`](docs/decisions/README.md) — ADR index and conventions

## Initial compatibility

Initial development targets compatibility with **Oracle Database 19c+**. This is compatibility information only and is not part of the Reldex brand.

## License

No project license has been selected yet. The repository should remain private until the Community/Pro licensing model is finalized.
