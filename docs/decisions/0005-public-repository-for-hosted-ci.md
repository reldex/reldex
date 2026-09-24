# 0005 — Public repository to keep hosted CI

**Status:** Accepted — owner decision 2026-09-24
**Date:** 2026-09-24

## Context

The `reldex` GitHub account is on the Free plan: 2,000 Actions minutes/month for a **private**
repository, with OS multipliers on top of wall-clock minutes (macOS ×10, Windows ×2, Linux ×1).
Every PR runs 13 jobs across the three workflows:

- `ci.yml`: `test` × 3 OS (Windows/Linux/macOS) + `header` (1 job) = 4 jobs
- `ui.yml`: `ffi-smoke` × 3 OS, `qt-build` × 3 OS, `qt-asan` (ubuntu) = 7 jobs
- `mobile-cross-compile.yml`: `android-arm64`, `ios-arm64` = 2 jobs

≈ 40 billable minutes per PR once the multipliers are applied (the macOS legs alone — `test`,
`ffi-smoke`, `qt-build` — dominate that number at ×10). Since 2026-09-19 the repository had run
≥ 100 workflow runs, and the account's minutes were exhausted on **2026-09-21**. Every job
submitted since then has failed within ~2 seconds with *"The job was not started because recent
account payments have failed or your spending limit needs to be increased."* PRs #22, #24 and #25
were merged between 2026-09-21 and 2026-09-24 on local verification only (`cargo fmt`/`clippy`/
`test` run by hand on the Windows dev machine, plus the relevant `oracle-it` suite where
applicable) — CI showed red/not-started the whole time, and that was known and accepted, not
silently ignored.

`AGENTS.md` ("Testing") requires the smallest relevant test set, then the broader affected suite,
after every change; it does not by itself mandate a *hosted* three-OS matrix, but the existing
`ci.yml`/`ui.yml`/`mobile-cross-compile.yml` workflows are the only place Linux, macOS, ASan/UBSan/
LSan, and Android/iOS cross-compile ever get exercised — none of that runs on the Windows dev
machine. Losing hosted CI for an extended period means losing that coverage, not just losing a
convenience.

The owner was offered five options on 2026-09-24: reduce the job matrix, add a self-hosted runner,
stop using Actions entirely (verify locally instead, hosted CI dormant), wait for a billing fix, or
make the repository public. The owner's first choice, recorded briefly earlier the same day, was
**stop using Actions** — but once the per-run cost breakdown above was laid out (macOS's ×10
multiplier is most of the 40 minutes, and a **public** GitHub repository gets Actions minutes for
free with no monthly cap on the Free/Team plans, unlike a private one), the owner changed the
decision to **go public** instead, so the existing three-OS matrix keeps running exactly as
designed rather than losing coverage.

## Decision

**The repository becomes public.** This is a billing fix, not an architecture or verification
change: `.github/workflows/ci.yml`, `ui.yml` and `mobile-cross-compile.yml` are **unchanged** —
same triggers (`push`/`pull_request` on the paths they already watch), same jobs, same three-OS
matrix, same ASan/UBSan/LSan coverage on Linux, same Android/iOS cross-compile checks. **Hosted CI
remains the merge gate.**

`tools/gates.sh` (added in this change) is a **local pre-push / pre-PR** script — `cargo fmt`,
`cargo clippy` (workspace, the `oracle-thin` `oracle-it` feature build, and the `reldex-ffi`
`--no-default-features` build), `cargo test --workspace`, the `crates/ffi/gen-header.sh --check`
header check, a `cargo doc -D warnings` lint scoped to the crates that are clean today, and, when
discoverable/healthy on the machine it runs on, the Qt UI build+test and the real-database
`oracle-it` integration suite. Its Markdown summary line is meant to be pasted into the PR body as
a second, earlier signal — it does not replace a green CI run, and nothing in this decision asks
reviewers to treat it as one.

The visibility flip itself is **not yet executed** as of this ADR: it is gated on an exposure
review of the full commit history (secrets, credentials, internal-only material).

**Exposure scan 2026-09-24:** full history, all refs — no credentials, keys or certificates ever
committed (`tools/oracle-test-db/.env` and the TCPS wallet directories are correctly `.gitignore`d;
only `.env.example`, which carries placeholders, is tracked); no files larger than 5 MB anywhere in
history; one Android device serial (`docs/exec-plans/active/phase-0-android-device.md`, the "Device
and environment" table) is redacted at HEAD but remains present in earlier commits — accepted as
low risk (a device serial identifies a specific owner-controlled test phone, not a credential or a
person). The README's earlier "remain private until licensing is decided" note is superseded by
this ADR.

## Licence

**Decision:** the Community edition is **GPL-3.0-or-later**. Copyright holder: **Supawit Nu-iat**
("Copyright (C) 2026 Supawit Nu-iat", `COPYRIGHT` at the repository root; full text in `LICENSE`,
fetched verbatim from GitHub's licence API — not retyped). Every crate's `Cargo.toml` carries
`license = "GPL-3.0-or-later"` and `repository = "https://github.com/reldex/reldex"` via
`[workspace.package]` inheritance. **Reldex Pro will be offered separately by the copyright
holder** and is not covered by this GPL grant.

**Rationale:**

- **Copyleft protects the Community edition from closed forks.** A GPLv3 licence means a
  downstream fork of Community must itself stay open and GPL-compatible; it cannot be taken closed-
  source and resold by a third party. The copyright holder retains the separate right to license
  Pro commercially under different terms, since GPL only binds licensees, not the copyright holder.
- **Compatible with the existing dependency graph.** Qt is used under LGPLv3 with dynamic linking
  only (`docs/exec-plans/active/phase-1.md` §C.0 "Licence position") — LGPLv3 code linked
  dynamically by a GPLv3 application is a standard, compatible combination; nothing here asks Qt's
  LGPL terms to become GPL terms. The Rust dependency graph is overwhelmingly permissively licensed
  (MIT/Apache-2.0), which GPLv3 can incorporate freely. The primary database driver, `oracledb`
  (`oracle/rust-oracledb`), is `UPL-1.0 OR Apache-2.0` (confirmed via `cargo metadata`) — both
  permissive and GPLv3-compatible.
- **A CLA is needed before accepting outside contributions.** Going public opens the door to
  external pull requests; without a contributor licence agreement, the project cannot re-license
  contributed code later (e.g. to adjust the Community/Pro boundary) without tracking down every
  contributor's consent individually. No CLA process exists yet — this is a prerequisite for
  accepting any external PR, not merely a nice-to-have, and is recorded as an open item rather than
  silently assumed.

Third-party notices for the full dependency graph (Rust crates plus Qt's own third-party content)
are generated at packaging time (`TASKS.md` M6.5), not retyped by hand here.

## Consequences

**What this fixes.** Actions minutes stop being a merge-blocking resource: GitHub does not cap
Actions minutes for public repositories on the Free/Team plans, so the existing 40-minutes/PR
matrix keeps running at no billable cost, without reducing OS coverage or adding self-hosted
runner infrastructure to maintain.

**What becomes true once the repository is flipped, that was not true as a private repo:**

- **The exposure scan must pass first.** Making a repository public cannot be undone by making it
  private again in the eyes of anyone who already cloned or indexed it; the scan is a precondition,
  not a formality, and this ADR's decision is not "go public now" but "go public once the scan
  clears."
- **Forks and PRs from outside contributors become possible.** GitHub's "require approval for
  first-time contributors" setting for Actions must be (and, per GitHub's default for newly-public
  repositories, is expected to be) **on** after the flip, so an unknown contributor's workflow run
  does not execute against repository secrets without a maintainer's review first. This could not
  be verified before the flip — the setting is private-repository-inapplicable (GitHub's API
  rejects the query with "Fork PR approval is not allowed for private repositories") — so it is a
  post-flip checklist item, not something this ADR can confirm today.
- **Upstream issue filings may now name the project.** The five upstream `oracle/rust-oracledb`
  issues filed 2026-09-19 (#21–#25, tracked in ADR-0001) did not need to identify Reldex by name;
  future upstream interactions may, once the repository (and any issue that links back to it) is
  discoverable. Not a blocker, but worth the reporter's awareness per the project's own upstream
  issue etiquette (dedupe, add context, owner reviews drafts before posting).
- **A stated licence now exists (see "Licence" above), settled the same day as this ADR.**
  Anyone browsing the public repository has a clear, granted right to use and redistribute
  Community under GPL-3.0-or-later; Reldex Pro is explicitly carved out as separately licensed.
- **The commit history, including anything already committed before this decision, becomes
  public exactly as it stands.** The exposure scan gate above is the control for this; nothing in
  this ADR substitutes for it.

**What does not change.** The verification matrix, its OS coverage, its ASan/UBSan/LSan job, its
mobile cross-compile checks, and its status as the merge gate are all unchanged from before this
decision. `tools/gates.sh` is a net addition (faster local feedback before pushing), not a
replacement for anything CI already did.

## Alternatives considered

1. **Reduce the job matrix** (fewer OS legs, drop `qt-asan` or the mobile cross-compile jobs from
   the routine PR path, keep them `workflow_dispatch`-only). Rejected: this is exactly the coverage
   loss (Linux, macOS, ASan/UBSan/LSan, Android/iOS cross-compile) that "keep CI as-is via a public
   repo" avoids for the same or lower cost once minutes are free.
2. **Add a self-hosted runner.** Rejected for now: real infrastructure and maintenance burden
   (patching, security exposure of a runner with repository access, uptime) to solve a problem a
   visibility change solves for free; not ruled out for the future if the repository needs to stay
   private for other reasons.
3. **Stop using Actions entirely, verify locally instead (`tools/gates.sh` as the merge gate,
   hosted workflows dormant via `workflow_dispatch`-only triggers).** This was the owner's first
   choice earlier on 2026-09-24, and `tools/gates.sh` was originally built to be exactly that. It
   was superseded, in the same conversation, once the per-run billing breakdown showed a public
   repository keeps the full three-OS/ASan/mobile-cross-compile matrix at no cost — which is a
   strictly better outcome than accepting a Linux/macOS/ASan/iOS coverage gap. `tools/gates.sh` is
   kept regardless, repurposed as a pre-push local check rather than the gate itself.
4. **Wait for the billing issue to resolve** (fix the payment method / raise the spending limit).
   Rejected as the sole path: it leaves the account exhausted-minutes-prone again the next time PR
   volume spikes, where going public removes the ceiling entirely.
5. **Make the repository public** (chosen). Fixes the root cause (a private-repo minutes cap) at
   its source, preserves the full existing verification matrix, and costs nothing recurring —
   conditional on the exposure scan clearing and the licence/first-time-contributor items above
   being handled honestly rather than glossed over.

## Evidence/benchmarks

Job/minute accounting is arithmetic from the workflow definitions (`.github/workflows/*.yml`) and
GitHub's published Free-plan multipliers (macOS ×10, Windows ×2, Linux ×1), not a benchmark. PR
merge history (`gh pr view 22/24/25 --json mergedAt,state`) confirms all three merged 2026-09-24
while CI was down, on local verification only.
