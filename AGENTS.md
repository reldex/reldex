# Reldex Agent Instructions

Keep this file concise. Detailed product requirements live in `SPEC.md`, architecture details in `docs/architecture/ARCHITECTURE.md`, and current work in `TASKS.md` plus `docs/exec-plans/active/`.

## Read before coding

For any non-trivial change, read:

1. `SPEC.md`
2. `docs/architecture/ARCHITECTURE.md`
3. the relevant active plan under `docs/exec-plans/active/`
4. any ADR related to the area being changed

Do not invent product requirements that conflict with these files.

## Non-negotiable architecture rules

- Reldex product/core layers are vendor-neutral.
- Vendor-specific database behavior belongs under drivers/providers.
- Rust Core is UI-independent.
- Qt/QML is presentation; business rules do not belong in QML.
- Keep the C++/Qt adapter thin.
- Never perform database/network I/O on the UI thread.
- A worksheet owns a stable stateful database session.
- Do not silently replace a worksheet session.
- Auto-commit defaults to OFF.
- Never silently commit or hide transaction loss.
- Large results must be cursor/batch based and virtualized.
- Never create one QML object per row for a large result.
- Mobile and Desktop share the same database Core.
- Mobile direct-connect is not considered supported until physical-device validation passes.

## Code quality

- Prefer explicit, typed APIs over stringly typed cross-layer protocols.
- Avoid unnecessary allocation and copying on result-processing paths.
- Keep driver-specific types inside the driver implementation.
- Normalize external driver errors into the internal error model while preserving native database error codes.
- Do not log passwords, credentials, private keys, or tokens.
- Do not add production dependencies without documenting why they are required.
- Keep public APIs small until the architecture stabilizes.

## Testing

After code changes, run the smallest relevant test set and then the broader affected suite.

A change is incomplete if:
- behavior changed without tests where tests are practical;
- a performance claim has no benchmark;
- database behavior was assumed rather than integration-tested;
- mobile support is claimed from emulator/simulator-only results.

Integration tests that require a real database must be clearly separated from unit tests.

## Performance

Do not optimize from intuition alone.

When changing result handling, FFI boundaries, rendering models, metadata loading, or concurrency:
- record a baseline;
- measure CPU, memory, allocation, latency, or frame time as appropriate;
- keep benchmark results with the relevant task/plan when they influence an architectural decision.

## Documentation

Update documentation in the same change when behavior or architecture changes.

Create an ADR under `docs/decisions/` when a decision:
- changes a core boundary;
- changes session/transaction semantics;
- changes the database-driver strategy;
- changes Qt/Rust integration;
- changes result-storage architecture;
- changes cross-platform support expectations.

## Naming and branding

- Product name: `Reldex`.
- Do not place database vendor trademarks in the Reldex brand, executable branding, app identifier, domain, or logo.
- Vendor names are allowed in technical compatibility/driver contexts where necessary.
- Generic modules use names such as `DatabaseSession`, `DatabaseDriver`, `MetadataProvider`.

## Scope discipline

Do not implement future roadmap items merely because they are easy.

For Phase 0:
- prioritize driver/core feasibility;
- do not build the full editor or polished UI;
- resolve architecture gates before downstream work.

If a requirement is unclear and materially changes architecture, stop and document the ambiguity rather than guessing.
