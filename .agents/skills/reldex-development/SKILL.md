---
name: reldex-development
description: Develop, review, debug, benchmark, or plan changes for the Reldex repository. Use for Reldex Rust core, database drivers, Qt/QML UI, C++/Rust FFI, session/transaction behavior, result virtualization, mobile direct-connect validation, architecture decisions, and implementation planning. Do not use for unrelated repositories or generic database questions that do not modify or analyze Reldex.
---

# Reldex Development Skill

Use this skill for repository work on Reldex.

## Before doing work

Read, in order:

1. `AGENTS.md`
2. `SPEC.md`
3. `docs/architecture/ARCHITECTURE.md`
4. `TASKS.md`
5. the relevant plan under `docs/exec-plans/active/`
6. related ADRs under `docs/decisions/`

If the requested change conflicts with those sources, call out the conflict instead of silently choosing a new architecture.

## Core invariants

Preserve these unless an explicit ADR changes them:

- The product/core are vendor-neutral.
- Database-vendor code stays in drivers/providers.
- The Rust Core is UI-independent.
- QML contains presentation logic, not database business logic.
- Keep the C++/Qt adapter thin.
- Database/network I/O never runs on the UI thread.
- A worksheet owns a stable stateful database session.
- Auto-commit defaults to OFF.
- Never silently commit.
- Never silently replace a lost transactional session.
- Large results are batch/cursor based and virtualized.
- Mobile and Desktop share the same Core.
- Mobile direct-connect support requires physical-device evidence.

## Workflow for implementation tasks

1. Identify the affected layer and invariant.
2. Check whether an ADR is required.
3. Make the smallest coherent change.
4. Add or update tests.
5. Run relevant tests/checks.
6. For performance-sensitive paths, capture a baseline and benchmark the change.
7. Update documentation/task status when behavior or architecture changed.
8. Summarize risks, test evidence, and any unresolved assumptions.

## Database-specific work

For initial Oracle Database compatibility:

- keep native error codes available for diagnostics;
- do not leak driver-native structs into generic APIs;
- verify session/transaction behavior with integration tests;
- test LOB, NUMBER precision, DATE/TIMESTAMP, binds, REF CURSOR, cancellation, and network-loss behavior explicitly;
- treat permissions-dependent metadata/monitoring behavior separately from driver failures.

## Result-processing work

Never solve large-result problems by materializing everything into UI objects.

Prefer:

```text
Cursor → Batch → Result Store → Virtual Model → Visible Cells
```

When proposing Arrow or another representation, justify it with measured allocation/memory/throughput improvements.

## Qt/QML work

- Prefer model/view interfaces over pushing large JS/QML arrays.
- Do not block the GUI thread.
- Avoid per-cell heavyweight QML objects when a lighter delegate/model can work.
- Keep platform-specific behavior behind the adapter/platform layer.
- Preserve keyboard-first desktop UX while allowing adaptive tablet/phone layouts.

## Mobile work

Do not claim support because code cross-compiles.

Require physical-device evidence for:
- connect;
- SQL/PLSQL;
- transactions;
- cancellation;
- LOB;
- TCPS;
- background/resume;
- lost-session/reconnect semantics.

## Documentation updates

If a change affects:
- product behavior → update `SPEC.md`;
- architecture → update `docs/architecture/ARCHITECTURE.md` and add/update an ADR;
- execution scope/status → update `TASKS.md` and the active plan;
- progress/plan/ADR status → update `Task.html` (data block only) per `AGENTS.md`.

See `references/review-checklist.md` before declaring a substantial task complete.
