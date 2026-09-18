# Reldex

**Reldex** is a high-performance, cross-platform database development environment designed for developers and DBAs.

The product is desktop-first, with tablet/mobile support planned around the same database core. Reldex is vendor-neutral at the product and core layers; database-specific support is implemented through drivers and providers.

Reldex is an independent project and is not affiliated with, endorsed by, or sponsored by Oracle Corporation. Oracle is a registered trademark of Oracle and/or its affiliates.

## Current status

**Phase 0 — Architecture Validation.** The project is proving the Rust core, thin database connectivity, cancellation, transaction behavior, and mobile feasibility before any significant UI investment (`ROADMAP.md`).

What exists today:

- Product/architecture documentation (`SPEC.md`, `docs/architecture/ARCHITECTURE.md`, `ROADMAP.md`, `TASKS.md`, the active Phase 0 plan).
- [ADR-0001](docs/decisions/0001-database-driver-strategy.md) — **Accepted, conditional on spikes S1–S5**: the primary database driver is Oracle's official `oracledb` crate (`oracle/rust-oracledb`).
- A Cargo workspace skeleton (`crates/db-driver-api`, `crates/db-core`, `crates/drivers/mock`, `crates/drivers/oracle-thin`, `crates/reldex-core-poc`) and GitHub Actions CI (`cargo fmt`, `cargo clippy`, `cargo test` on Windows/Linux/macOS).
- A local Oracle 19c Docker test database under [`tools/oracle-test-db/`](tools/oracle-test-db/), verified (AL32UTF8, Thai round-trip, Non-CDB).
- [ADR-0002](docs/decisions/0002-driver-api-and-concurrency-model.md) (driver API and concurrency model) — **Accepted (provisional — implemented; independent API review in progress; owner review pending)**. The `db-driver-api` contract is implemented: 5 traits, ~45 types, zero production dependencies, 68 unit + 6 doc tests, fmt/clippy/test green.

What does not exist yet:

- No UI (Qt Quick/QML has not been started).
- No database driver implementation — `db-driver-api` now has the vendor-neutral trait contract and normalized `DbError` (ADR-0002), but `db-core`'s session/transaction implementation, the mock driver, and the `oracledb` wrapper in `crates/drivers/oracle-thin` are all still to do.
- No mobile validation — Android/iOS direct-connect feasibility has not been tested on physical devices.

[`Task.html`](Task.html) (open it in a browser) is the live, human-facing progress dashboard: current focus, blockers/risks, spike results, and recent activity. [`TASKS.md`](TASKS.md) and the active plan under [`docs/exec-plans/active/phase-0.md`](docs/exec-plans/active/phase-0.md) remain the source of truth for task status; `Task.html` mirrors them.

## Technology decisions so far

- **Core:** Rust.
- **UI:** Qt Quick/QML with a thin C++ adapter over a stable Rust FFI boundary — planned, not started.
- **Primary database driver:** Oracle's official [`oracledb`](https://github.com/oracle/rust-oracledb) crate (pure Rust, thin, blocking; no Instant Client/OCI required), pinned to an exact pre-GA beta version, per [ADR-0001](docs/decisions/0001-database-driver-strategy.md). It is encapsulated behind `db-driver-api` so it can be swapped if a kill criterion in the ADR's spike plan fires.
- **Known gaps in the primary driver**, stated honestly: no public statement-cancel API yet (Phase 0 may fall back to call-timeout semantics while an upstream request is pending); it is pre-GA/beta software; Native Network Encryption and 11G password verifiers are unsupported; Android/iOS viability is unproven and requires physical-device evidence before any mobile claim.
- **Initial compatibility target:** Oracle Database 19c+.

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
  db-core/                sessions, transactions, query, results, metadata, workspace (skeleton)
  drivers/mock/            test-support/mock driver for core tests (skeleton)
  drivers/oracle-thin/     thin driver wrapping Oracle's `oracledb` crate, ADR-0001 (skeleton)
  reldex-core-poc/         Phase 0 validation harness binary (no full UI)
docs/
  architecture/            architecture source of truth (ARCHITECTURE.md)
  decisions/               ADRs (see docs/decisions/README.md)
  exec-plans/active/       current execution plan(s), e.g. phase-0.md
tools/
  oracle-test-db/          local Oracle 19c Docker test database for integration tests
.agents/skills/            repository-local skill shared by all coding agents (reldex-development)
.claude/skills/            Claude Code entry point; thin wrapper around .agents/skills
.github/workflows/         CI (fmt, clippy, test)
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
