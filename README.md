# Reldex

**Reldex** is a high-performance, cross-platform database development environment designed for developers and DBAs.

The product is desktop-first, with tablet/mobile support planned around the same database core. Reldex is vendor-neutral at the product and core layers; database-specific support is implemented through drivers and providers.

## Current status

**Phase 0 — Architecture Validation**

The project is currently validating:

- Rust as the application core
- Qt Quick/QML as the presentation layer
- a thin C++/Qt adapter around a stable Rust FFI boundary
- direct database connectivity through a pure/thin driver
- Oracle Database 19c as the initial compatibility target
- Android ARM64 and iOS ARM64 feasibility before committing to mobile UI work

Do not treat mobile direct-connect as supported until the Phase 0 physical-device gates pass.

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

## Repository map

- [`SPEC.md`](SPEC.md) — product and technical specification
- [`TASKS.md`](TASKS.md) — current project task board
- [`ROADMAP.md`](ROADMAP.md) — development phases
- [`AGENTS.md`](AGENTS.md) — repository rules for coding agents
- [`docs/architecture/ARCHITECTURE.md`](docs/architecture/ARCHITECTURE.md) — architecture source of truth
- [`docs/exec-plans/active/phase-0.md`](docs/exec-plans/active/phase-0.md) — active execution plan
- [`.agents/skills/reldex-development/SKILL.md`](.agents/skills/reldex-development/SKILL.md) — repository-local Codex/ChatGPT skill

## Initial compatibility

Initial development targets compatibility with **Oracle Database 19c+**. This is compatibility information only and is not part of the Reldex brand.

Reldex is an independent project and is not affiliated with, endorsed by, or sponsored by Oracle Corporation. Oracle is a registered trademark of Oracle and/or its affiliates.

## License

No project license has been selected yet. The repository should remain private until the Community/Pro licensing model is finalized.
