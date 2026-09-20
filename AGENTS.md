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

## Scripts and shell

Project scripts are bash-first (owner decision 2026-09-20).

- The `.sh` script is the primary, tested, documented entry point. It must run under Git Bash on Windows as well as on Linux and macOS.
- A PowerShell twin (`.ps1`) is optional; when one exists it must stay in step with the `.sh` script.
- Documentation leads with the bash command.
- Mind Git Bash path conversion when calling native Windows tools (`cygpath`, `MSYS_NO_PATHCONV=1`), and never echo credentials (`set +x`, no environment dumps).
- Scripts set `PATH` and tool variables for the current process only; they never edit user or machine configuration.

## Documentation

Update documentation in the same change when behavior or architecture changes.

Create an ADR under `docs/decisions/` when a decision:
- changes a core boundary;
- changes session/transaction semantics;
- changes the database-driver strategy;
- changes Qt/Rust integration;
- changes result-storage architecture;
- changes cross-platform support expectations.

## Progress dashboard (`Task.html`)

`Task.html` at the repository root is the human-facing dashboard for project progress, latest status, and the current plan. It is a single self-contained file (no build step, no network access) that the owner opens directly in a browser.

- `TASKS.md` and the active plan under `docs/exec-plans/active/` remain the source of truth for task status; `Task.html` mirrors them and adds the status narrative (current focus, blockers/risks, next steps, decisions, recent activity).
- Update `Task.html` **in the same change** whenever any of these change: a task status in `TASKS.md` or the active plan, an ADR is added or changes status, a phase gate or spike result is reached, a blocker appears or is resolved, or the plan changes.
- Edit only the embedded data block (`<script type="application/json" id="project-data">`); do not restyle or restructure the page as part of routine updates. Always refresh `updatedAt` and prepend an entry to the activity log.
- Report honestly: failed spikes, blocked work, and known limitations must be visible on the dashboard, never omitted.
- A change that alters progress or plans but leaves `Task.html` stale is incomplete.

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
