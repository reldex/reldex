# Reldex Review Checklist

Use before declaring a substantial task complete. Source: `AGENTS.md`, `SKILL.md`, `TASKS.md` task rules.

## Scope & spec alignment

- [ ] Change maps back to `SPEC.md`; no invented requirements that conflict with it.
- [ ] Phase 0 work prioritizes driver/core feasibility, not full editor/polished UI.
- [ ] No downstream UI work started while a Phase 0 architecture gate is unresolved.
- [ ] Unclear requirements that would materially change architecture are documented, not guessed.

## Architecture invariants

- [ ] Product/core layers remain vendor-neutral.
- [ ] Vendor-specific database behavior stays under drivers/providers.
- [ ] Rust Core remains UI-independent.
- [ ] QML stays presentation-only; no business rules in QML.
- [ ] C++/Qt adapter stays thin.
- [ ] Mobile and Desktop still share the same database Core.
- [ ] Public APIs kept small until the architecture stabilizes.

## Session/transaction safety

- [ ] A worksheet still owns one stable, stateful database session.
- [ ] No worksheet session silently replaced.
- [ ] Auto-commit still defaults to OFF.
- [ ] No silent commit; no hidden transaction loss.

## Driver boundary & errors

- [ ] Driver-specific types stay inside the driver implementation.
- [ ] External driver errors normalized into the internal error model, native error codes preserved.
- [ ] Cross-layer APIs stay explicit/typed, not stringly typed protocols.

## Result/performance

- [ ] Large results remain cursor/batch based and virtualized (Cursor → Batch → Result Store → Virtual Model → Visible Cells).
- [ ] No large result materialized entirely into UI objects.
- [ ] Unnecessary allocation/copying avoided on result-processing paths.
- [ ] Arrow (or other representation) changes justified with measured allocation/memory/throughput data.
- [ ] Performance-sensitive changes have a recorded baseline and measurement (CPU/memory/allocation/latency/frame time), kept with the task/plan.

## Qt/QML & threading

- [ ] No database/network I/O on the UI thread.
- [ ] No one-QML-object-per-row for large results; model/view or delegate used instead.
- [ ] GUI thread not blocked.
- [ ] Platform-specific behavior kept behind the adapter/platform layer.
- [ ] Keyboard-first desktop UX preserved alongside adaptive tablet/phone layouts.

## Mobile claims

- [ ] Mobile direct-connect not claimed as supported without physical-device validation.
- [ ] Support not claimed merely because code cross-compiles.
- [ ] Physical-device evidence covers: connect, SQL/PLSQL, transactions, cancellation, LOB, TCPS, background/resume, lost-session/reconnect.

## Security/logging

- [ ] No passwords, credentials, private keys, or tokens logged.

## Tests

- [ ] Smallest relevant test set run, then the broader affected suite.
- [ ] Tests added/updated where behavior changed, or a documented reason given for their absence.
- [ ] Database behavior integration-tested, not assumed.
- [ ] Integration tests requiring a real database kept separate from unit tests.
- [ ] Mobile support not claimed from emulator/simulator-only results.

## Docs/ADR/task updates

- [ ] Documentation updated in the same change as the behavior/architecture change.
- [ ] ADR added under `docs/decisions/` if the change affects a core boundary, session/transaction semantics, driver strategy, Qt/Rust integration, result-storage architecture, or cross-platform support expectations.
- [ ] `SPEC.md` updated when product behavior changed (if this task owns that file).
- [ ] `docs/architecture/ARCHITECTURE.md` updated when architecture changed (if this task owns that file).
- [ ] `TASKS.md` and the active exec plan updated when execution scope/status changed.
- [ ] Any new production dependency documented with a reason.
