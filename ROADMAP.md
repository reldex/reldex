# Reldex Roadmap

## Phase 0 — Architecture Validation

Prove the Rust core, thin database connectivity, cancellation, transaction behavior, and mobile feasibility before substantial UI investment.

**Exit gate:** Desktop POC passes. Android/iOS feasibility is documented from physical-device tests.

## Phase 1 — Windows Desktop MVP

Deliver a useful developer workflow:

- connections
- SQL/PLSQL worksheets
- independent sessions
- result grid
- transactions
- cancellation
- DBMS_OUTPUT
- basic object browser
- workspace/history

## Phase 2 — Professional IDE

Add:

- PL/SQL source editing/compilation
- table inspector
- explain plan
- metadata cache
- autocomplete
- export
- macOS/Linux support

## Phase 3 — Performance & DBA

Add:

- session/lock/wait monitoring
- long operations
- SQL statistics
- plan analysis
- large-result optimization
- benchmark-driven Arrow evaluation

## Phase 4 — Tablet

Bring the same Core to Android tablets and iPad with a desktop-like adaptive workspace.

## Phase 5 — Phone

Adapt presentation without creating a separate database backend.

## Phase 6 — Pro

Candidate commercial features:

- schema/data compare
- advanced monitoring
- performance analysis
- automation
- advanced export
- AI
- optional gateway/team features

No Pro feature should weaken Community architecture or duplicate the core.
