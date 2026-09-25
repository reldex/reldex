# Spike S15 — Qt ↔ Rust boundary measurement (M1.8)

ADR-0003 is **Proposed**, and its acceptance is gated on this spike. The kill
criteria K1–K7 and their thresholds were fixed in `docs/decisions/0003-qt-rust-integration.md`
before anything was measured; nothing here adjusts one.

Everything below was measured on **2026-09-20** on the dev machine, against the
real vertical slice ADR-0003 asks for: real `reldex-ffi` (ABI 3), the committed
cbindgen header, the Corrosion build, the real `QAbstractTableModel`, the real
wake → `invokeMethod` path, and the mock driver producing 1,000,000 rows of the
S14 shape (`NUMBER`, `VARCHAR2(40)`, `DATE`). No row count was reduced and
presented as 1,000,000; no frame was skipped; the delegate really renders text.

Where something could not be measured, it says so, and says why.

## Summary

Verdict cells use exactly one of **PASS**, **FAIL**, **NOT MEASURED** or
**PASS-with-named-gap**. No threshold was adjusted.

| # | Criterion | Threshold | Measured | Verdict |
| --- | --- | --- | --- | --- |
| K1 | Scroll frame time over the full 1M rows, 100% and 150% | p99 > 16.7 ms, or p50 > 8 ms | **Cost** (production, pacing artefact removed): worst p50 **5.72 ms**, worst application CPU **5.24 ms**, worst p99 14.62 ms. **Budget** (vsync on, display awake, 36,008 frames): p50 16.67–16.73 ms, p99 17.26–19.03, **8 frames over 33 ms (0.022%), 2 of them in the idle control** | **PASS-with-named-gap**: passes the p50 clause on measured cost and the p99 clause on the dropped-frame reading; the *literal* p99 reading fails for an idle window too, so **which quantity K1's p99 names needs an owner ruling** |
| K2 | Execute → first row painted, mock driver, in-process | > 150 ms | Cold process, vsync on (n=30): **257.33 ms to rows, 903.55 ms to the painted frame**. Warm (2nd..31st, n=30): 1.34 ms / **15.85 ms**. The cold cost is now apportioned: ~250 ms of D3D11 device creation and **one 551 ms `polishItems` pass** — delegate instantiation, identical at 1,000 and 1,000,000 rows, absent at 1 row, 7–10 ms on the second execute | **FAIL** as the ADR is written (it has no warm/cold qualifier). Warm passes by ~9×. Cause located **outside** the boundary; **needs an owner ruling** on whether K2 means the warm path |
| K3 | Retained memory, 1M rows of the S14 shape | > 200 MB RSS growth | Headless **+114.1…118.9 MB RSS** (114–119 B/row), n=3. In-app from an idle drawn app **+172.5…172.7 MB RSS**, private +142.0–142.1 MB, n=3. **From process start: +200.3…200.8 MB RSS and +198.4…199.3 MB private** | **PASS** on the metric of record (rows' cost, 172.7 MB); the most conservative reading is **marginal and crosses 200 × 10⁶ B** — disclosed in K3 |
| K4 | Event delivered → batch described and columns viewable; per-cell `data()` | > 200 µs; or per-cell avg > 200 ns | `applyBatch` headless **p50 0.90 µs, p99 2.9 µs**; in-app **p50 2.1–5.3 µs, p99 15.2–79.8 µs**, worst single batch **257.9 µs** (vsync on) / 791.4 µs (unthrottled render loop). The boundary's own share of a drain: **p50 0.4–2.0 µs, p99 3.1–7.1 µs — under 1%** of a drain's 80–250 µs. Warm per-cell: **99.7–120.4 ns** | **PASS** at p50/p90/p99 and per cell; the worst single batch exceeds 200 µs, which is reported rather than filtered |
| K5 | Waker teardown, 10,000 iterations, ASan | any use-after-free, race, or hang | Here, 3 runs: **10,000 iterations in 6.30 / 6.35 / 6.70 s**, no hang, no crash, `reldex_live_counts` back to baseline every time. Under **ASan+UBSan+LSan on Linux CI**: 10,000 iterations in 13.13 s, **zero sanitizer reports**, live counts back to baseline | **PASS-with-named-gap**: no sanitizer on the measured platform; ASan/UBSan/LSan clean on Linux CI |
| K6 | UI never stalls under a 10 s-class blocking statement | any frame > 33 ms attributable to the boundary | Frames, vsync on, n=3 processes: **1 frame over 33 ms in 9,012, in the phase with nothing blocked**. "Any other session": a third hub's 100,000-row stream took **866.3–867.4 ms with another session blocked and 866.3–867.4 ms without**, n=15 per side. Scrolling *while* streaming: 2 frames over 33 ms in 601, boundary share 0.8% | **PASS** on both clauses, **plus NOT MEASURED** for two sessions on one hub (not reachable in this adapter) |
| K7 | CMake+Corrosion+Qt builds and offscreen tests pass on all three runners | fails on any of the three, or a cold job over 25 min | Cold (first run, no caches): windows **3.0 min**, ubuntu **2.1 min**, macos **1.6 min but FAILED**. Warm: windows 2.4–4.0, ubuntu 1.3–2.2, macos 0.8–1.2 min; the new `qt-asan` ubuntu job 2 min 21 s. Current `main` green on all three | **PASS**, with the one historical macOS failure named (Qt 6.8.3 build-script race, fixed in M1.4) and green since |

**No criterion's failure is located in the boundary.** K2's cold reading is
Qt-side: ~250 ms of D3D11 device creation plus a single 551 ms delegate-
instantiation polish, with the boundary contributing 2.2 ms of it. K7's
first-ever macOS run was a build-script race fixed inside M1.4. **Three
questions are now the owner's rather than this report's**: what K1's p99 names,
whether K2 is about the warm path, and which baseline and metric K3 is judged
on.

## Environment

Recorded read-only; **no system setting was changed** — not display scale, not
the power plan, not a GPU setting — and nothing was installed.

| | |
| --- | --- |
| CPU | AMD Ryzen 7 5700G, 8 cores / 16 threads, 3.80 GHz max |
| RAM | 63.4 GB |
| OS | Windows 11 Pro, build 26200 (10.0.26200) |
| GPUs | AMD Radeon integrated (driver 31.0.21921.1000, 2024-08-19) driving the **primary** display; NVIDIA GeForce RTX 3060 Ti (driver 32.0.16.1047, 2026-05-19) driving the second |
| Displays | primary "LG HDR WFHD" 2560×1080 @ **59.978 Hz** (the window ran here); second 1920×1080 @ 60 Hz |
| System scale | **96 DPI = 100%**. The 150% runs use `QT_SCALE_FACTOR=1.5` (`devicePixelRatio` 1.5 confirmed in the run's own JSON) — this **emulates** 150%; it is not a system-DPI run, and the owner's display setting was not touched |
| Power | Desktop, on AC, **Balanced** scheme as found; display-off timeout 60 s (AC) |
| Graphics API | **D3D11** (`QSGRendererInterface::graphicsApi`), **threaded** render loop (frames arrive on a thread other than the GUI thread; `QSG_RENDER_LOOP` unset) |
| Qt | 6.8.3 (LGPL, dynamically linked) |
| Build | **Release** (`bash ui/build.sh --release`), MSVC 19.44.35222, `reldex-ffi` cargo `--release` |
| Rust | rustc 1.98.1, cargo 1.98.1 |
| CMake / Ninja | 4.4.3 / 1.13.2 |
| Commit | **Round 1** (K3, K4, K5, the sweeps, the K1 `NO_VSYNC` tables): measurement code as committed in `2743d93`, on branch `phase-1/m1-8-s15` over base `daa8d67`. **Round 2** (every vsync-on number, the GUI-thread and drain-split numbers, K6's other-session phases, the first-frame breakdown): `5b50cbf` (= `2743d93` merged with `main` at `5770185`) **plus the uncommitted deltas listed in "What this task added"** — `Metrics` gained the GUI-thread and drain-boundary brackets and a corrected first-frame latch, `ScrollDriver` gained the `streambase`/`streamblocked`/`streamscroll` phases, `Bridge` times the boundary's share of a drain, `main.cpp` gained the `RELDEX_S15_LOG` sink |
| Window | 962×665 logical at 100%; 961×657 logical / 1.5× device pixels at 150% |

**Two measurement sessions, and the difference between them matters.**

- **Round 1** ran with the displays asleep, which throttled the Windows desktop
  compositor to ~4 Hz for every application on the machine. Not an inference:
  `DwmFlush()` — which blocks until the next desktop composition — returned with
  a **median of 251.55 ms** (min 250.28, max 265.46) when probed from a separate
  process. No vsync-on frame number could be taken, so K1 round 1 was answered
  from frame production with vsync off.
- **Round 2** ran with the display awake. The same probe returned **median
  16.69 ms** (min 14.68, max 18.07) — a live 60 Hz compositor — so the vsync-on
  measurement round 1 could not make is in this report, and it is what K1's
  budget clauses are now judged on. Nothing was done to wake the display: no
  input was synthesized and no power, display or GPU setting was changed; the
  desktop was simply in use again between the two rounds.

Every table below says which round it comes from.

## K1 — scroll frame time

### Method

`ui/adapter/ScrollDriver.{h,cpp}`, new in this task and **off unless an
environment variable asks for it**, drives the real window on the real
swapchain. It runs a named phase for a fixed number of frames, discards a
warm-up window, and records every `QQuickWindow::frameSwapped` interval through
`Metrics`. Four motion patterns plus a control:

| Phase | What it does | Why |
| --- | --- | --- |
| `idle` | no motion at all | the control: what a frame costs when the application does nothing |
| `flick` | a flick-shaped velocity profile on `contentY` (initial 9,000 px/s, Flickable's own 1,500 px/s² deceleration, re-kicked when it dies) | the rows-per-frame rate a real flick or wheel produces (~3.5 rows/frame) |
| `sweep` | constant 110 px/frame (5 rows), reversing at the ends | a steady drag |
| `full` | `contentY` 0 → max → 0 with the step sized so one down-and-up pass takes the phase | **traverses the entire 1,000,000 rows**; every frame lands in a different batch and a different formatted window |
| `jump` | a uniformly random `contentY` every frame | the worst case for the windowed format cache: no frame can reuse the previous frame's window |

3,000 recorded frames per phase, 60 warm-up frames discarded, after the whole
1,000,000-row result has streamed and memory has settled for 3 s. Each phase
reports the `contentY` it actually reached, because a pattern that silently
fails to move produces excellent-looking frame times — which is exactly what
happened on the first attempt (see "What went wrong").

**What each pattern is and is not evidence for, before any number is read.**

- **`jump` is the defensible worst case and the one the verdict leads with.**
  It has no tunable that changes its cost: every frame picks a uniformly random
  `contentY`, so every frame lands in a cold formatted window with zero
  delegate reuse regardless of how many frames the phase runs for.
- **`full`'s per-frame cost is a function of a parameter of this measurement,
  not a property of the application.** Its step is
  `contentMaxY / (RELDEX_S15_SCROLL_FRAMES / 2)`, so halving the frame budget
  doubles the rows per frame: at 3,000 frames it moves 667 rows/frame and at
  600 it moves 3,333. Any `full` number is therefore meaningless without the
  frame count next to it, and both counts are reported below.
- **No phase literally flick-scrolls a million rows, and none could.** `flick`
  covers **10,424 rows — about 1% of the result** — in 3,000 frames, because
  that is what a flick at a realistic velocity actually covers; scrolling all
  1,000,000 rows at a human flick rate would take roughly 48 minutes of
  continuous flicking. `full` and `jump` traverse the whole result (or sample
  it uniformly) by moving hundreds or thousands of rows per frame, which is
  *harsher* per frame than any human input: no delegate is reused and most
  frames start from a cold format window. They bound K1's criterion from above;
  they are not a simulation of a user.

### Two things the driver itself puts into a frame, and how they were removed

Both were found after round 1, by an independent review of this report, and
both mattered enough to invalidate a central claim of its first version.

**1. `QT_QPA_UPDATE_IDLE_TIME` — the "5.31 ms presentation floor" was neither.**
The round-1 report said the ~5.3 ms that every vsync-off phase cost, including
an idle window, was "a fixed presentation floor this machine imposes". That is
wrong. It is the driver's own pacing: `ScrollDriver` asks for the next frame
with `QQuickWindow::requestUpdate()`, which on Windows reaches
`QPlatformWindow::requestUpdate()`, whose idle timer defaults to **5 ms** and is
not overridden by the Windows QPA plugin. The wait is paid once per frame and
lands *inside* the measured interval. Measured A/B on this build, 600 frames per
phase, vsync off, 1,000,000 rows:

| Phase (600 frames, vsync off, 100%, minutes apart) | timer at its 5 ms default | timer set to `0` | the timer's share |
| --- | --- | --- | --- |
| `idle` p50 | 5.255 ms | **0.215 ms** | −5.04 ms |
| `flick` p50 | 5.263 ms | **0.900 ms** | −4.36 ms |
| `sweep` p50 | 5.267 ms | **1.185 ms** | −4.08 ms |
| `full` p50 | 9.706 ms | **5.615 ms** | −4.09 ms |
| `jump` p50 | 9.479 ms | **5.248 ms** | −4.23 ms |

So the round-1 vsync-off rows for `idle`, `flick` and `sweep` (5.31 / 5.33 /
5.34 ms) carry **almost no information about this application** — they are
mostly a timer. Those rows are kept below, marked, under "the pacing artefact";
the frame-production numbers K1 is judged on come from passes with the timer
disabled.

**The subtraction the round-1 report did was illegitimate, and it flattered the
result.** It said "7.75 − 5.31 = about 2.5 ms of application work". The idle
wait and the application's work *overlap* — the timer is a lower bound on the
interval, not an additive term — so subtracting it understates the application's
cost, in the direction that makes the verdict look better. Measured on the A/B
above: the subtraction would give `full` 9.706 − 5.255 = **4.45 ms**, while the
same phase with the timer removed actually costs **5.615 ms** — the subtraction
understates it by 1.17 ms (21%), and by more at other frame budgets. It is
removed. K1's `p50` clause is now judged on a measured quantity (GUI-thread work
plus render-thread work, from a pass with no pacing wait in it), not on a
subtraction.

**A second thing the timer was hiding: `full`'s apparent sensitivity to the
frame budget.** With the timer on, `full` reads p50 7.75 ms at 3,000 frames per
phase (667 rows/frame) and 9.71 ms at 600 (3,333 rows/frame), which looks like
"the number depends on how far each frame jumps". (Those two are from different
sessions — 7.75 ms is round 1, under the throttled compositor — so the gap
between *them* is not clean; what follows is, because both halves are round 2,
minutes apart.) With the timer off it reads
**5.595 ms at 3,000 frames and 5.615 ms at 600** — the same, to 20 µs. That is
the expected result once it is stated properly: both step sizes move the
viewport further than its own height, so both pay exactly one cold format window
and one full delegate rebuild, and how many rows were skipped in between costs
nothing. `full`'s frame cost is dominated by fixed per-frame work, not by the
distance travelled. The frame count still belongs next to any `full` number, and
it is given.

**2. The GUI thread's half of a frame was not measured at all.** Round 1 timed
`beforeSynchronizing → afterRendering`, which is the *render* thread's scene-graph
work. `TableView`'s polish, delegate creation and reuse, every `data()` call and
therefore the whole windowed bulk formatter run on the **GUI** thread, outside
that bracket. `Metrics` now also records `previous swap → afterAnimating` (the
GUI thread's share, which contains the pacing wait when there is one),
`afterAnimating → beforeSynchronizing` (the handover) and
`beforeSynchronizing → afterSynchronizing` (the sync), so a per-frame cost can
be stated as a sum of measured parts rather than as half of one.

### Two passes, and which clause each one can answer

K1 has two clauses in one sentence, and **no single pass can answer both**.

- With **vsync on**, a swap interval quantizes to the refresh period: p50 is
  16.67 ms at 60 Hz whether the application did nothing or traversed a million
  rows. That pass can say *whether a frame made its budget*; it cannot say what
  a frame cost.
- With **vsync off and the pacing timer disabled**, the scene graph presents as
  soon as it is ready (`QSurfaceFormat` swap interval 0 →
  `QRhiSwapChain::NoVSync`), so the interval is the application's own production
  time. That pass can say *what a frame cost*; it says nothing about budgets.

So: **the `p99` / dropped-frame clause is judged on the vsync-on pass, and the
`p50 > 8 ms` clause on the frame-production pass.** Both are below, both at both
scales, and each number says which question it answers.

Round 1 could only run the second pass, because the desktop compositor was
throttled to ~4 Hz (displays asleep) and every phase — including a window doing
nothing — swapped at 252 ms. Round 2 ran with the display awake
(`DwmFlush` median 16.69 ms) and both passes are here.

### Results — vsync on, display awake, 1,000,000 rows (round 2)

This is the pass that answers "did frames make their budget". 3,000 recorded
frames per phase, 60 discarded.

100% scale (`devicePixelRatio` 1):

| Phase | frames | p50 | p90 | **p99** | max | > 16.7 ms | > 20 ms | **> 33 ms** | GUI p50 / p99 | render p50 / p99 | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` *(control)* | 3,000 | 16.671 | 16.887 | 17.420 | 30.09 | 1,185 | 2 | **0** | 16.504 / 17.126 | 0.106 / 0.336 | 0 |
| `flick` | 3,001 | 16.666 | 16.954 | 17.554 | 31.07 | 1,280 | 2 | **0** | 16.415 / 17.175 | 0.553 / 1.329 | 10,424 |
| `sweep` | 3,001 | 16.680 | 17.100 | 17.882 | 28.95 | 1,402 | 4 | **0** | 16.387 / 17.455 | 0.652 / 1.674 | 15,005 |
| `full` | 3,000 | 16.702 | 17.502 | 18.490 | 32.68 | 1,507 | 2 | **0** | 3.280 / 5.715 | 1.911 / 3.579 | 2,000,615 |
| **`jump`** | 3,000 | 16.703 | 17.566 | 18.510 | 33.04 | 1,505 | 11 | **1** | 3.397 / 9.008 | 1.892 / 3.771 | 998,638,991 |

150% scale (`QT_SCALE_FACTOR=1.5`, `devicePixelRatio` 1.5 confirmed):

| Phase | frames | p50 | p90 | **p99** | max | > 16.7 ms | > 20 ms | **> 33 ms** | GUI p50 / p99 | render p50 / p99 | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` *(control)* | 3,001 | 16.677 | 16.997 | 17.619 | 33.20 | 1,359 | 2 | **2** | 16.509 / 17.417 | 0.117 / 0.428 | 0 |
| `flick` | 3,001 | 16.695 | 17.225 | 17.905 | 34.48 | 1,477 | 3 | **2** | 16.366 / 17.435 | 0.704 / 1.683 | 10,450 |
| `sweep` | 3,001 | 16.694 | 17.231 | 17.847 | 19.26 | 1,483 | 0 | **0** | 16.366 / 17.413 | 0.781 / 1.839 | 15,005 |
| `full` | 3,000 | 16.716 | 17.593 | 18.826 | **122.70** | 1,528 | 8 | **2** | 3.393 / 9.182 | 2.248 / 4.200 | 2,000,616 |
| **`jump`** | 3,000 | 16.734 | 17.710 | 19.025 | 32.76 | 1,542 | 9 | **0** | 3.474 / 10.162 | 2.243 / 4.363 | 998,639,445 |

A second, harsher pair at **600 frames per phase** (so `full` moves 3,333
rows/frame instead of 667) is in `k1-vsync-frames.csv`: at 100% `full` peaked at
23.90 ms with 0 frames over 33 ms and `flick` had one at 34.20 ms; at 150%
nothing anywhere exceeded 26.89 ms.

**What "dropped frame" means here, and what the numbers say.** At 59.978 Hz a
frame that misses its deadline is presented one period later, so a *dropped*
frame shows up as a swap interval of about two periods — **> 33 ms**, which is
also K6's threshold. Across all four vsync-on runs, **8 frames out of 36,008
(0.022%) exceeded 33 ms**, and they are distributed `idle` 2, `flick` 3, `full`
2, `jump` 1. **Two of them are in the control phase, where the application does
no row work at all**, so this rate is the machine's, not the application's: at
this frequency the patterns are indistinguishable from doing nothing. The one
outlier that is *not* explained that way is `full`'s **122.70 ms** frame at
150%, whose GUI-thread bracket was 119.41 ms — see "the one frame that is not
noise" below.

### Results — frame production (vsync off, `QT_QPA_UPDATE_IDLE_TIME=0`), round 2

This is the pass that answers "what did a frame cost". Same 3,000 frames per
phase.

100% scale:

| Phase | frames | **p50** | p90 | p99 | max | > 8 ms | **GUI work p50 / p99** | **render work p50 / p99** | **GUI+render p50** | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` *(control)* | 3,000 | **0.201** | 0.428 | 3.474 | 6.75 | 0 | 0.060 / 0.437 | 0.032 / 0.179 | **0.09** | 0 |
| `flick` | 3,000 | **1.054** | 2.534 | 5.812 | 19.74 | 11 | 0.699 / 4.378 | 0.426 / 1.478 | **1.13** | 10,424 |
| `sweep` | 3,001 | **1.172** | 2.148 | 4.854 | 18.64 | 10 | 0.869 / 2.283 | 0.458 / 1.469 | **1.33** | 15,005 |
| `full` | 3,000 | **5.595** | 7.506 | 13.768 | 34.72 | 192 | 3.294 / 9.210 | 1.841 / 3.780 | **5.14** | 2,000,615 |
| **`jump`** | 3,000 | **5.495** | 7.051 | 8.376 | 15.32 | 60 | 3.248 / 5.520 | 1.779 / 3.322 | **5.03** | 998,638,991 |

150% scale:

| Phase | frames | **p50** | p90 | p99 | max | > 8 ms | **GUI work p50 / p99** | **render work p50 / p99** | **GUI+render p50** | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` *(control)* | 3,000 | **0.181** | 0.357 | 4.389 | 6.66 | 0 | 0.058 / 0.359 | 0.031 / 0.175 | **0.09** | 0 |
| `flick` | 3,001 | **0.916** | 1.902 | 3.651 | 9.47 | 1 | 0.641 / 2.276 | 0.361 / 1.456 | **1.00** | 10,450 |
| `sweep` | 3,001 | **1.247** | 2.216 | 3.749 | 10.10 | 2 | 0.926 / 2.594 | 0.501 / 1.571 | **1.43** | 15,005 |
| `full` | 3,000 | **5.716** | 7.426 | 8.996 | 14.92 | 123 | 3.374 / 5.843 | 1.868 / 3.718 | **5.24** | 2,000,616 |
| **`jump`** | 3,000 | **5.492** | 7.178 | 8.814 | 11.44 | 82 | 3.303 / 5.865 | 1.750 / 3.587 | **5.05** | 998,639,445 |

"GUI work" is previous swap → `afterAnimating`: `TableView`'s polish, delegate
creation and reuse, and every `data()` call, i.e. the whole windowed formatter.
With the pacing timer disabled there is no wait inside it, so it is work.
"Render work" is `beforeSynchronizing` → `afterRendering`. The sum is the
application's own per-frame CPU cost with neither a present nor a wait in it;
the p50 column above it, which is the whole interval, is 0.3–0.5 ms larger
because it also contains the handover and the present.

`rows travelled` is derived from the pixels the viewport actually moved divided
by the row height the view reports, so "2,000,615" is the whole result scrolled
down and back, not a claim about a shorter run.

### Results — the pacing artefact (round 1), kept and marked

These are the round-1 tables. Every row in them contains the ~4–5 ms
`QT_QPA_UPDATE_IDLE_TIME` wait described above, **and** was taken while the
desktop compositor was throttled, so they are superseded by the two passes above
and are retained only so that what this report said in round 1 can be checked
against what corrected it. **No verdict is taken from them.**

| Phase | 100% p50 / p99 / max | 150% p50 / p99 / max |
| --- | --- | --- |
| `idle` — *pacing timer only, no application work* | 5.31 / 6.31 / 6.65 | 5.38 / 6.42 / 6.72 |
| `flick` — *pacing-limited* | 5.33 / 6.49 / 8.42 | 5.36 / 6.49 / 9.04 |
| `sweep` — *pacing-limited* | 5.34 / 6.48 / 7.88 | 5.38 / 6.52 / 8.75 |
| `full` — *pacing-inflated* | 7.75 / 10.31 / 14.56 | 7.69 / 10.19 / 12.40 |
| `jump` — *pacing-inflated* | 7.82 / 10.84 / 11.91 | 7.74 / 10.20 / 11.62 |

### The one frame that is not noise

`full` at 150%, vsync on, produced a single **122.70 ms** frame in 3,000. Its
GUI-thread bracket was **119.41 ms**, so the stall was on the GUI thread, not in
the scene graph (render work p99 in the same phase was 4.20 ms) and not at the
boundary (no drain in that run exceeded 4.1 ms, and the boundary's own p99 was
4.8 µs). It did not recur: the same phase at 100% peaked at 32.68 ms, and the
600-frame pair peaked at 23.90 and 26.89 ms.

**It is not attributed further, because attributing it would be a guess.** The
candidates are the QML engine's garbage collector, a formatted-window cache
eviction burst, and a delegate-pool rebuild; separating them needs a sampling
profiler over the GUI thread, which this task did not run. It is recorded as an
open item rather than explained away.

### Verdict and how to read it

- **`p50 > 8 ms` clause — PASS, on measured cost rather than on a subtraction.**
  The worst frame-production p50 is **5.72 ms** (`full`, 150%) and the worst
  application CPU cost (GUI + render) is **5.24 ms**, against an 8 ms threshold.
  `jump`, which is the parameter-free worst case, is 5.49/5.50 ms. The margin is
  about 30%, and it is smaller than the round-1 report implied — that report's
  "about 2.5 ms of application work" was arithmetic on a pacing timer and is
  withdrawn.
- **`p99 > 16.7 ms` clause — the literal reading fails for every phase
  including the control, so it cannot be the reading intended.** With vsync on
  at 60 Hz, p50 *is* 16.67 ms, so any jitter at all puts p99 above 16.7: the
  measured p99 is **17.26–17.62 ms for a window doing nothing** and 19.03 ms for
  the worst pattern. On the reading the clause is plainly about — did frames
  miss their 60 Hz budget — the answer is **PASS**: 8 frames over 33 ms in
  36,008 (0.022%), of which 2 are in the control phase, and the patterns are
  statistically indistinguishable from an idle window. **This needs an owner
  ruling on which quantity K1's p99 names** (a swap interval, or the frame's
  cost), exactly as K2 needs one on warm-versus-cold. On the frame-cost reading
  the answer is also PASS: the worst production p99 is 14.62 ms.
- **150% is not materially more expensive.** Frame-production p50 differs by
  under 0.25 ms from 100% in every phase, and render work rises from 1.84 to
  1.87 ms p50 in `full`. Cost here is scene-graph bookkeeping and cell
  formatting, not fill rate.
- **What scrolling a million rows actually costs**: about **5.5 ms of CPU per
  frame** in the harshest pattern versus **1.2 ms** for a steady drag and
  **0.2 ms** for an idle window. Roughly 60% of that is on the GUI thread
  (polish, delegates, `data()`, the bulk formatter) and 33% on the render
  thread. The boundary's share is not separately visible at this scale — it is
  microseconds (see K4).

## K2 — first-row latency

### Method

`Metrics` marks four points per run: execute submitted (in
`SessionController::execute`, immediately before `reldex_session_execute`),
first event drained, first rows inserted into the model, and the first
`frameSwapped` after that insert — the last taken on the render thread, which
is where the frame actually was. `ScrollDriver` ends a run at the first painted
frame and, for the warm case, submits the next execute **synchronously** so no
batch of the result being replaced can land between clearing the marks and
recording the next execute.

Three variants, all against the 1,000,000-row query:

- **cold process**: 30 separate process launches, execute submitted from
  `Component.onCompleted`;
- **cold process, window already drawn**: 10 launches with the first execute
  held back 1.5 s, so the app is idle and has rendered before Execute;
- **warm**: one process, 31 executes, entries 2–31 counted.

### Results

Round 1 was measured with vsync off; round 2 repeated it with vsync on, which
is the configuration the product ships, and with the corrected first-frame latch
(see "What went wrong", item 7).

| Variant | round | n | execute → rows in model | execute → **first frame painted** |
| --- | --- | --- | --- | --- |
| warm (2nd..31st execute) | 1, vsync off | 30 | p50 **1.30 ms**, p95 1.56, max 1.57 | p50 **11.16 ms**, p95 12.30, max 12.35 |
| **warm (2nd..31st execute)** | **2, vsync on** | **30** | p50 **1.34 ms**, p95 3.50, max 12.24 | p50 **15.85 ms**, p95 19.77, max 34.00 |
| cold process, window already drawn | 1, vsync off | 10 | p50 **2.01 ms**, p95 2.29, max 2.29 | p50 **431.82 ms**, p95 459.68, max 459.68 |
| **cold process, window already drawn** | **2, vsync on** | **10** | p50 **2.23 ms**, p95 5.84, max 5.84 | p50 **549.42 ms**, p95 640.53, max 640.53 |
| cold process, execute at startup | 1, vsync off | 30 | p50 **190.65 ms**, p95 198.22, max 202.47 | p50 **621.97 ms**, p95 810.77, max 811.47 |
| **cold process, execute at startup** | **2, vsync on** | **30** | p50 **257.33 ms**, p95 311.00, max 323.84 | p50 **903.55 ms**, p95 979.23, max 1,551.91 |

Vsync costs the warm case **4.7 ms** — a little under one 16.7 ms refresh
period, which is what it should cost. It costs the cold case far more (622 →
904 ms), and the reason is in the diagnosis below: the cold path is not one
expensive frame, it is roughly a dozen frames, and vsync puts 16.7 ms between
each of them.

### Diagnosis — what the cold first paint actually is

Against the 150 ms threshold the warm number passes by ~9×, and the cold number
fails. Round 1 could only say the cold cost was "fixed, once per process, not
the boundary" and explicitly declined to apportion it. Round 2 apportions it,
from Qt's own instrumentation (`QT_LOGGING_RULES="qt.scenegraph.time.*=true"`,
captured through an env-gated file sink because `Reldex.exe` is a GUI-subsystem
binary with no console).

1. **The boundary's own contribution is 1.3–2.3 ms.** Execute submitted →
   1,000 rows described, marshalled, crossed, and inserted into the model, in
   under 2.3 ms in every variant where the GUI thread is free.
2. **The 190–257 ms in the cold rows column is contention with application
   startup, not work.** It disappears entirely (2.01 ms) when the execute is
   held back 1.5 s. Submitting a query from `Component.onCompleted` means the
   reply cannot be drained until the QML engine and window finish initializing.
   Qt's own log shows what the GUI thread is doing meanwhile: it is
   `blockedForSync` for **249–271 ms** while the render thread creates the
   `QRhi`/D3D11 device and enumerates DXGI adapters.
3. **The ~432–580 ms before the first painted frame is one `polishItems` pass
   on the GUI thread, and it is delegate instantiation.** Qt reports it
   directly:

   | Run | first-execute frame | second execute | third .. sixth |
   | --- | --- | --- | --- |
   | 1 row | `polish=0 ms` / `1 ms` | — | — |
   | 1,000 rows | **`polish=551 ms`** | — | — |
   | 1,000,000 rows | **`polish=551 ms`** | — | — |
   | 1,000 rows, six executes in one process | **`polish=579 ms`** | `polish=9 ms` | `7 / 10 / 7 / 8 ms` |

   Three facts fall out of those four rows, and together they identify it:
   it is **identical at 1,000 and 1,000,000 rows** (so it is not row work); it
   is **absent at 1 row** (so it is the first *screenful* of delegates, not the
   first row); and it **drops to 7–10 ms on the second execute in the same
   process** (so it is one-time). That is the `TableView` instantiating its
   first viewport of delegates and running its first layout inside
   `QQuickWindowPrivate::polishItems`.

   The same logs rule out the other candidates by their own numbers: the render
   thread's whole contribution to those frames is `sync=4..12, render=2..5,
   swap=1..4` — under 20 ms; the distance-field glyph cache prepares 32 glyphs
   in 1 ms; the texture atlas is created once at 1024×1024. **None of the cost
   is on the render thread and none of it is shader or pipeline creation.**

**Fix candidates, named and not implemented** (this task measures; ADR-0003
allows an adapter-side fix only for a *failing* criterion with an obvious
adapter-side cause, and this is a Qt-side startup cost, not a boundary one):

- **Warm the delegate before the first result** — instantiate the `TableView`'s
  delegate once during startup (an empty or one-row placeholder model, or
  `Component.incubateObject`) so the one-time instantiation happens while the
  user is still choosing a connection rather than on their first query.
- **Asynchronous delegate creation** — `Loader.asynchronous` / incubation on the
  delegate spreads the same work over several frames instead of one 551 ms
  polish. It does not reduce the total; it removes the stall.
- **`TableView.reuseItems`** (already the Qt 6 default) is what makes the
  *second* execute cost 7–10 ms; nothing to do there.
- **Not** the QML disk cache: `Main.qml` is ahead-of-time compiled into the
  binary by `qmlcachegen` (the build produces `.rcc/qmlcache/Reldex_Main_qml.cpp`),
  so this is instantiation, not compilation.
- **Separately, the 250 ms of D3D11 device creation** is startup cost that
  `SPEC.md` §19's "warm desktop startup < 1 second" has to budget for, and it is
  paid before any query.

**One environmental finding that belongs with every frame number in this
report.** Qt's D3D11 backend logged `Adapter 0: 'NVIDIA GeForce RTX 3060 Ti' …
using this adapter`, while the window ran on the 2560×1080 display, which
`Win32_VideoController` reports as driven by the **AMD Radeon integrated GPU**.
So the scene graph renders on the discrete GPU and presents to a display
attached to the other one. Whether that costs anything measurable here was not
isolated; it is recorded because it is a property of this machine that another
machine may not share.

**Verdict: PASS on the metric that describes the product** (a running worksheet
where the user presses Execute: 11.16 ms), **FAIL on the literal cold-process
reading**, with the cause outside the boundary. ADR-0003 says a K1/K3 failure
diagnosed in the model rather than the boundary is a design fix and not a kill;
it does not say so about K2, which is why this is written up for the lead and
owner to rule on rather than resolved here.

## K3 — retained memory for 1,000,000 rows

### Method

Two environments, because they answer different questions and only one of them
is the boundary's.

- **Headless** (`tst_resultmodel spikeS15BoundaryCost`, no scene graph, no
  window): baseline immediately before the stream, sampled again once the
  result is complete. This is the data pipeline alone.
- **In-app** (the real window, scene graph running): baseline taken when the
  app is **idle and has already drawn**, 2.5 s after start and immediately
  before the first execute; sampled again 5 s after the result completed. Three
  runs each. ("Memory flat" was claimed in an earlier draft of this report and
  is withdrawn: the driver samples memory at the settle point and at each phase
  boundary, which is not enough points to call a curve flat.)

Both working set (`WorkingSetSize`) and private bytes (`PrivateUsage`) are
recorded, from **both** baselines, and all four numbers are in the table. ADR
K3's own words are "RSS growth", so the verdict is on **working set measured
from the idle, already-drawn app** — see "which number is the metric of record"
below, which argues the choice instead of assuming it.

### Results — all four readings, not the flattering one

| Environment | run | baseline | **RSS growth** | B/row | private growth | B/row |
| --- | --- | --- | --- | --- | --- | --- |
| headless | 1 | before the stream | +115.97 MB | 116.0 | +117.94 MB | 117.9 |
| headless | 2 | before the stream | +118.92 MB | 118.9 | +121.66 MB | 121.7 |
| headless | 3 | before the stream | +114.11 MB | 114.1 | +114.90 MB | 114.9 |
| in-app | 1 | **idle app, already drawn** | **+172.7 MB** | 172.7 | +142.1 MB | 142.1 |
| in-app | 2 | **idle app, already drawn** | **+172.7 MB** | 172.7 | +142.0 MB | 142.0 |
| in-app | 3 | **idle app, already drawn** | **+172.5 MB** | 172.5 | +142.0 MB | 142.0 |
| in-app | 1 | process start | +200.7 MB | 200.7 | +198.8 MB | 198.8 |
| in-app | 2 | process start | +200.8 MB | 200.8 | +198.4 MB | 198.4 |
| in-app | 3 | process start | +200.3 MB | 200.3 | +199.3 MB | 199.3 |
| in-app | K1 vsync-on rerun | process start | +200.7 MB | 200.7 | +199.2 MB | 199.2 |

**Verdict: PASS** on the metric of record — worst reading 172.7 MB against a
200 MB threshold, reproducible to within 0.2 MB across three runs.

**And the most conservative reading crosses the line, so it is stated first
rather than buried.** Measured from *process start*, the same runs read
**+200.3…200.8 MB of working set** and +198.4…199.3 MB of private bytes. If
"200 MB" is read as 200 × 10⁶ bytes, the working-set-from-process-start reading
is **over the threshold by 0.3–0.8 MB**; if it is read as 200 MiB
(209,715,200 B) nothing crosses. The ADR does not say which, and this report
does not pick for it. Both readings are on the table above; either way the
number is *marginal*, and a reader who wants the safe statement should take it
as "the whole process, window and driver included, sits within a percent of
200 MB for a million rows".

**Which number is the metric of record, and why.** K3 asks what 1,000,000 rows
*retain*. A baseline at process start charges the rows with every one-time cost
the application pays anyway — the QML engine, the scene graph, the RHI, the
graphics driver's own allocations — which on this machine is ~28 MB of working
set and ~57 MB of private bytes and does not grow with the row count. That is
why the idle-drawn baseline is the one the verdict uses. It is also why the
process-start reading is reported next to it: the difference is large enough
(28 MB) to flip the verdict, and a report that showed only the smaller number
would be choosing its own result.

Three honest notes:

- **Working set is not a well-behaved metric** and both directions of that are
  visible here: it is trimmable by the OS and counts shared pages, which is why
  in-app RSS growth (172.7 MB) is *larger* than in-app private growth
  (142.1 MB). Private bytes is the number that says what this process made the
  system commit. The ADR names RSS; if the owner would rather K3 be judged on
  private bytes, the in-app figure is 142.0–142.1 MB and the margin is wider,
  not narrower.
- The headless figure (114–119 B/row) is essentially the ADR's own ~116 B/row
  estimate, and confirms M1.6's numbers after the ABI 3 change that stopped
  `reldex_batch_column` retaining unread `NUMBER`/`TIMESTAMP` mirrors
  (amendment A19). The in-app figure is ~55 B/row higher. **That ~55 B/row
  delta was not attributed by measurement** — headless and in-app differ by the
  scene graph, the view, the formatted-window cache and the QML engine all at
  once, and nothing here separates them. Calling it "the scene graph's and the
  view's" would be an inference, so it is not claimed.
- What is retained is still the whole fetched prefix (ADR-0003 D4's MVP rule).
  K3 passes *with* that; the result store (ADR-0004) is what changes it.

## K4 — per-batch boundary cost and per-cell `data()`

### Method

K4 has two halves in different units, and they are measured separately.

**Per batch — and which interval is timed, argued rather than assumed.**
K4's words are "event delivered → batch described and its columns viewable".
Three nested intervals could claim that sentence, and all three are now
measured, because picking the smallest one silently would be choosing a
flattering answer:

| Interval | What is inside it | Where it is timed |
| --- | --- | --- |
| **boundary** | `reldex_hub_next_event` plus taking ownership of what the event carried, summed over one drain | `Bridge::drain()` |
| **`applyBatch`** | the call that takes the `ReldexBatch` and calls `reldex_batch_column` once per column, after which every column is viewable | `SessionController` |
| **drain** | the whole time the UI thread is held: boundary + dispatch + `applyBatch` + `beginInsertRows`/`endInsertRows` and every signal they emit | `Bridge::drain()` |

**`applyBatch` is the interval the criterion names**, and it is the one the
verdict is given on: it is exactly "the batch is described and its columns are
viewable", and it ends before Qt's model/view machinery starts. But the number
a UI engineer cares about is the **drain**, because that is what the UI thread
actually loses per batch — and most of a drain is *not* the boundary. Both are
below. All clock reads are behind the same off-by-default flag as the recorder.

**Per cell:** `data()` for a viewport-sized block, one column kind at a time,
because `TEXT` is read straight out of borrowed UTF-8 while `NUMBER` and `DATE`
come from the bulk formatter's windowed arena — one average over "a cell" would
hide the only distinction that matters. Cold (the window has never been
rendered) and warm (what a repaint pays) are reported separately. 8,000 warm
reads per column per run, 3 runs.

### Results — per batch, 1,000 batches of 1,000 rows

| Environment | p50 | p99 | max | mean |
| --- | --- | --- | --- | --- |
| headless, run 1 | 0.90 µs | 2.9 µs | 25.8 µs | 1.11 µs |
| headless, run 2 | 0.90 µs | 2.7 µs | 16.1 µs | 1.04 µs |
| headless, run 3 | 0.90 µs | 2.8 µs | 19.5 µs | 1.08 µs |
| in-app, round 1, all five runs (scene graph running, vsync off) | 2.1–2.9 µs | **15.2–17.5 µs** | **20.2–170.6 µs** | 3.3–4.0 µs |
| in-app, round 2, four runs (scene graph running, vsync **on**) | 4.2–5.3 µs | 57.9–79.8 µs | 177.1–257.9 µs | — |

The round-1 in-app range is over all five in-app runs that recorded it
(`k1-100-novsync`, `k1-150-novsync`, `k3-app-1..3`), not the three the first
draft of this report quoted. The round-2 numbers are higher because those runs
had vsync on, so the GUI thread is competing with a 60 Hz present cadence rather
than running flat out; they are reported because they are the worse of the two
and the verdict should be given on the worse one.

Against the **200 µs** threshold: the worst single batch anywhere, in-app,
under a live scene graph, was **257.9 µs** — *over* 200 µs, once, in one run of
1,000 batches — and the worst p99 was **79.8 µs**. **PASS at p50, p90 and p99**
(by 2.5× at the worst p99), with the single worst batch of ~5,000 recorded
in-app batches sitting above the line. A threshold on a maximum would fail here;
K4 is written on the cost of "a batch", the distribution is reported in full,
and the owner can decide whether the tail matters.

### Results — what a whole drain costs the UI thread (round 2)

Per drain of the hub's event queue, in-app, during the 1,000,000-row stream,
with the scene graph live:

| Run | drains | events | drain p50 | drain p99 | drain max | **boundary p50 / p99 / max** |
| --- | --- | --- | --- | --- | --- | --- |
| `k1v-100` | 826 | 1,004 | 102.2 µs | 881.5 µs | 2,671.7 µs | **0.6 / 4.6 / 29.9 µs** |
| `k1v-150` | 863 | 1,004 | 80.0 µs | 563.5 µs | 1,473.5 µs | **0.5 / 4.2 / 104.8 µs** |
| `k1v600-100` | 859 | 1,004 | 83.8 µs | 445.3 µs | 2,952.6 µs | **0.5 / 4.0 / 11.8 µs** |

**This is the number that justifies giving K4's verdict on `applyBatch`.** The
boundary itself — every `reldex_hub_next_event` call in a drain, plus taking
ownership of the batch — is **0.5–0.6 µs at p50 and ≤ 4.6 µs at p99**, i.e.
**under 1% of a drain**. `applyBatch` adds a few more microseconds. The
remaining ~95% of a drain's 80–102 µs is `beginInsertRows`/`endInsertRows` and
the signals they fan out to the view — Qt model/view work that would cost the
same against any data source. Choosing the drain as "the interval K4 names"
would therefore be measuring Qt and reporting it as the boundary's cost; that
is the argument for the narrower interval, and it is made from data rather than
asserted.

### Results — per cell

| Column | kind | cold first cell (renders a 1,024-row window) | **warm, ns/cell** |
| --- | --- | --- | --- |
| `ID` | `NUMBER`, bulk-formatted | 65.0 / 68.2 / 115.9 µs | **102.6 / 108.1 / 114.6** |
| `NAME` | `VARCHAR2(40)`, zero-copy UTF-8 | 1.2 / 1.2 / 1.3 µs | **99.7 / 101.7 / 107.7** |
| `CREATED` | `DATE`, bulk-formatted | 155.1 / 157.5 / 191.3 µs | **112.4 / 114.6 / 120.4** |

**Which number is judged against 200 ns, and why.** K4's second clause says
"per-cell `data()` **average**", and a repaint asks for every visible cell of an
already-rendered window — so the **warm** figure is the per-cell average, and
the worst of it is **120.4 ns** against 200 ns. **PASS.**

The cold first cell is not a per-cell cost at all: it renders one 1,024-row
window of one column through `reldex_batch_format_column`, and every one of the
next 1,023 cells in that window is then pointer arithmetic. Amortized it is
63–187 ns per cell, and taken as a *per-window* cost it sits under K4's 200 µs
per-batch budget (worst 191.3 µs) — so it passes on either reading, but it
passes the per-batch one with very little room. That is worth flagging to
whoever owns the `DATE` formatter: a wider date pattern, or a narrower format
window, would move it the wrong way.

The zero-copy text path is visible in these numbers: a cold `VARCHAR2` cell
costs 1.2 µs because nothing is rendered for it at all, which is D4 working as
designed.

## K5 — waker teardown under a flood

### Method

`ui/tests/tst_teardown.cpp` (M1.6, unchanged by this task), 10,000 iterations of
destroying the `Bridge` while completions are in flight, plus a 2,000-iteration
connect-window variant. Each iteration takes a `reldex_live_counts` baseline
before it starts and **waits** for the counts to return to it afterwards —
a wait, not an immediate compare, because `reldex_hub_destroy` does not join
the session pump threads (ADR-0003 A17). Run three times.

### Results

| Run | 10,000 iterations | per iteration | RSS across the loop | `reldex_live_counts` |
| --- | --- | --- | --- | --- |
| 1 | 6,301 ms | 0.630 ms | +2.79 MB | back to baseline (0 hubs / sessions / batches / errors / arenas) |
| 2 | 6,351 ms | 0.635 ms | +2.48 MB | back to baseline |
| 3 | 6,698 ms | 0.670 ms | +2.57 MB | back to baseline |

No hang, no crash, no leaked object, in any run. Total 4 tests passed in each.

### The sanitizer half, which is not from this machine

AddressSanitizer **cannot** run here: this Visual Studio 2022 Community install
ships only the 32-bit ASan runtime and the x64 counterpart is absent, so the
link fails with
`LNK1104: cannot open file 'clang_rt.asan_dynamic_runtime_thunk-x86_64.lib'`;
getting it would mean installing a Visual Studio component, which this task does
not do. **No ASan result is claimed from this Windows machine.**

It is covered on Linux instead. A `qt-asan (ubuntu-latest)` job — added by a
separate task in PR #19, merged to `main` after this worktree's base commit
`daa8d67` — runs `bash ui/build.sh --sanitize --test`, i.e. the **whole adapter
test suite including `tst_teardown`**, under AddressSanitizer, UndefinedBehavior‑
Sanitizer and LeakSanitizer. From CI run `35508957508` (job time 2 min 21 s):

```
K5: 10000 iterations in 13129 ms (1.313 ms/iteration);
    RSS 48734208 -> 484229120 bytes;
    live counts back to baseline (hubs=0 sessions=0 batches=0 errors=0 arenas=0)
```

The 2,000-iteration connect-window variant passed in the same job, there were
**zero AddressSanitizer, LeakSanitizer or UBSan reports**, and
`ui/tests/lsan.supp` contains no entries — so nothing was suppressed to get
there.

**On the 435 MB of RSS growth in that line, which looks alarming and is not** —
this is my reading of it, stated as an assessment rather than as a measurement I
made: ASan does not return freed memory to the allocator immediately, it holds
it in a quarantine precisely so that a later use-after-free lands on poisoned
pages instead of on recycled ones. Growth proportional to how much memory the
run churned is the expected shape, and three independent things say it is not a
leak: LeakSanitizer, which is the instrument that would report one, reported
nothing; `reldex_live_counts` returned to zero hubs, sessions, batches, errors
and arenas; and the same 10,000 iterations without ASan on this machine grow RSS
by **2.5–2.8 MB**, not 435 MB. If the quarantine were hiding a real leak, the
uninstrumented run is where it would show, and it does not.

**Verdict: PASS — with the qualifier attached to the verdict, not filed under
it: no sanitizer has been run on the platform this spike was measured on.** The
Windows evidence is 3 × 10,000 iterations with no hang, no crash and live counts
back to baseline; the ASan/UBSan/LSan evidence is from Linux CI over the whole
adapter suite and is clean. MSVC's ASan would exercise a different allocator and
a different threading runtime, so the two are not substitutes. Also in
Limitations.

## K6 — the UI never stalls

### Method

Four phases in one process, 3,000 frames each (16.2 s per phase, so the blocked
window comfortably exceeds the 10 s class the criterion names), all with the
full 1,000,000-row result loaded and the `sweep` motion running:

1. `sweep` — baseline, nothing blocked;
2. `blockother` — the same sweep while a **second `Bridge`** (a second hub, its
   own waker and its own pump thread) runs `RELDEX_MOCK_STATEMENT_BLOCK`, which
   parks that session's worker for the whole phase; the driver releases it at
   the end, so the blocked window *is* the measured window;
3. `sweep` — baseline again, to separate a real effect from drift;
4. `blocksame` — the block submitted on the **same** session that owns the
   result.

**The adapter's multi-session shape, stated rather than worked around.**
`Bridge` creates exactly one `SessionController` (its routing table handles any
number; only one is created — `ui/README.md` "Known limitations"), and a
session holds one result. So "another session" today means another `Bridge`,
which is what phase 2 does; and executing anything on the *same* session closes
the result it is showing, so in phase 4 the grid necessarily empties. Phase 4
therefore answers "does a blocking statement on my own session stall the UI
thread", and cannot answer "can I keep scrolling my rows while my own session
is busy" — that question needs the multi-result or multi-session work in M3.
That is a gap in the adapter, not a result.

### Method — the "or any other session" clause, measured

K6's threshold is about frames, but its sentence says the blocking statement
must not delay "any frame **or any other session**". Round 1 measured only the
frames. The second clause needs a session that is *trying to do work* while
another one is blocked, and a measurement of how long that work took — which is
what these two phases do, both while the window keeps sweeping the 1M-row
result:

| Phase | What runs | What is timed |
| --- | --- | --- |
| `streambase` | a **third** `Bridge`/hub executes the generated query and streams it to completion, five times in a row; nothing is blocked | each round's `execute` → `resultComplete` |
| `streamblocked` | the same five rounds on the third hub, while the **second** hub's session is parked in `RELDEX_MOCK_STATEMENT_BLOCK` for the whole phase | the same |

Three hubs, because three roles are needed at once and this adapter puts one
session in each `Bridge`: one holding the 1,000,000-row result the window is
scrolling, one blocked, one working. Each timed round streams 100,000 rows —
small enough that five rounds fit in a phase, and the number that matters is the
*difference* between the two phases, which uses the same row count on both
sides. n = 5 per phase, three repeats of the whole process (n = 15 per side).

**What this does not cover.** Both the blocked session and the working session
are on *separate hubs*, so they share no event queue, no waker and no pump
thread. The sharper question — two sessions on **one** hub, one of them blocked
— is **not reachable** in the M1.6 adapter at all, and is therefore **NOT
MEASURED** rather than answered by proxy. It belongs with the multi-session work
in M3.

### Method — scrolling *while* the result is still streaming

Round 1 scrolled only after `resultComplete`, so no measured frame ever
overlapped a drain, an `applyBatch` or an `endInsertRows`. That is not the
realistic worst case. The `streamscroll` phase re-executes on the window's own
session at the start of the measured window and then sweeps, so the timed frames
run *against* the arriving batches; the phase reports its own drain statistics
next to its own frame statistics.

### Results — frames (round 1, vsync off with the pacing timer; superseded)

| Phase | frames | p50 | p99 | max | > 16.7 ms | **> 33 ms** | model rows |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `sweep` (baseline) | 3,000 | 5.39 | 6.51 | 7.95 | 0 | **0** | 1,000,000 |
| `blockother` (second hub blocked 16.2 s) | 3,000 | 5.39 | 6.45 | 8.87 | 0 | **0** | 1,000,000 |
| `sweep` (baseline again) | 3,000 | 5.35 | 6.45 | 8.69 | 0 | **0** | 1,000,000 |
| `blocksame` (this session blocked) | 3,000 | 5.37 | 6.47 | 13.37 | 0 | **0** | 0 |

These carry the pacing wait described in K1, so their p50/p99 values are
inflated; the counts of frames over 33 ms are unaffected by it.

### Results — frames, vsync on, display awake (round 2, n = 3 processes)

600 recorded frames per phase per process, all with the 1,000,000-row result
loaded and the `sweep` motion running. Figures are the range across the three
processes.

| Phase | frames | p50 | p99 | max | > 20 ms | **> 33 ms** | model rows |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `sweep` (baseline) | 1,802 | 16.656–16.685 | 17.284–17.919 | 17.67–21.22 | 1 | **0** | 1,000,000 |
| `streambase` (third hub streaming, nothing blocked) | 1,803 | 16.673–16.695 | 17.461–17.810 | 18.21–33.29 | 2 | **1** | 1,000,000 |
| `streamblocked` (second hub blocked, third hub streaming) | 1,803 | 16.671–16.698 | 17.277–17.843 | 17.58–18.74 | 0 | **0** | 1,000,000 |
| `blockother` (second hub blocked) | 1,802 | 16.669–16.682 | 17.282–17.915 | 17.61–21.72 | 1 | **0** | 1,000,000 |
| `blocksame` (this session blocked) | 1,802 | 16.672–16.674 | 17.164–17.459 | 17.32–17.80 | 0 | **0** | 0 |

**One frame in 9,012 exceeded 33 ms, and it was in the phase with *nothing*
blocked** (`streambase`, 33.29 ms, GUI-thread bracket 33.10 ms) — the same
environmental hitch rate K1's idle control shows. Every phase with something
blocked was clean.

### Results — "or any other session", measured

Execute → `resultComplete` for a 100,000-row query on a third hub, five rounds
per phase, three processes, **n = 15 per side**:

| Phase | n | min | median | max | spread |
| --- | --- | --- | --- | --- | --- |
| `streambase` — nothing blocked | 15 | 866.3 ms | 866.8 ms | 867.4 ms | 1.1 ms |
| `streamblocked` — another session blocked for the whole phase | 15 | 866.3 ms | 866.8 ms | 867.4 ms | 1.1 ms |

The two distributions are **identical**: same minimum, same median, same
maximum, to 0.1 ms over 15 samples each, while the window kept scrolling a
million rows throughout both. A session parked in a blocking statement costs a
working session on another hub **nothing measurable** — and "nothing
measurable" here means below 0.13% of an 867 ms operation, not "we did not
look".

**Verdict: PASS on both clauses, with one clause narrower than it sounds.**
Frames: 1 frame over 33 ms in 9,012, in the unblocked phase, at the machine's
own hitch rate. Other sessions: no effect at n = 15 per side. The narrowing is
that "another session" means **another hub**, because this adapter puts one
session in each; two sessions sharing one hub's event queue and waker is
**NOT MEASURED** and not reachable here (see Method and Limitations).

`blocksame`'s grid necessarily empties — one result per session — so that phase
answers "does blocking my own session stall the UI thread" (it does not) and
cannot answer "can I keep scrolling my rows while my own session is busy".

### Results — scrolling while the result is still streaming

The `streamscroll` phase re-executes on the window's own session and then
sweeps, so every timed frame overlaps arriving batches. Vsync on, 600 recorded
frames per phase, bracketed by the same `sweep` with the stream finished:

| Phase | frames | p50 | p99 | max | > 20 ms | **> 33 ms** | GUI p50 / p99 / max | drains in the phase | drain p50 / p99 / max | boundary p50 / p99 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `sweep` before | 601 | 16.685 | 17.619 | 17.85 | 0 | **0** | 16.387 / 17.306 / 17.57 | 0 | — | — |
| **`streamscroll`** | 601 | 16.679 | 18.219 | **49.53** | 2 | **2** | 3.937 / 16.977 / 46.10 | 508 (1,004 events, 1,000 batches) | 249.5 / 791.0 / 1,155.0 µs | 2.0 / 5.7 µs |
| `sweep` after | 601 | 16.667 | 17.506 | 18.26 | 0 | **0** | 16.399 / 17.202 / 17.94 | 0 | — | — |

**This is the one configuration in this report that drops frames at a rate
above the machine's own.** Two frames in 601 exceeded 33 ms (0.33%, against
0.02% for everything else measured), with a worst frame of 49.53 ms whose GUI
bracket was 46.10 ms. That is the realistic worst case — a user scrolling a
result that is still arriving — and it was **not measured at all in round 1**,
where every timed frame came after `resultComplete`.

Where the time goes is visible in the same row: a drain costs **249.5 µs at
p50** while scrolling, against 98.0 µs for the same 1,000 batches with no
scrolling; the boundary's share of that is **2.0 µs** (0.8%), and `applyBatch`
is 8.5 µs. Across the phase, ~500 drains × ~250 µs ≈ 127 ms of the 10 s phase,
about 1.3% of the UI thread. **The frames are not being lost at the boundary**;
they are lost where the GUI thread has to do polish, delegate work and
`endInsertRows` in the same 16.7 ms window.

The same phase measured for *cost* rather than for budget (vsync off, pacing
timer off, 3,000 frames) says the work itself is not large:

| Phase | frames | p50 | p90 | p99 | max | > 8 ms | GUI p50 / p99 | render p50 / p99 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `sweep` before | 3,001 | 1.113 | 1.717 | 2.447 | 3.58 | 0 | 0.858 / 1.958 | 0.415 / 1.083 |
| **`streamscroll`** | 3,001 | **1.260** | 5.599 | **7.580** | 13.02 | 16 | 0.954 / 4.755 | 0.466 / 2.143 |
| `sweep` after | 3,001 | 1.141 | 1.836 | 2.759 | 9.83 | 2 | 0.869 / 2.070 | 0.427 / 1.131 |

Streaming under a scroll raises p99 frame cost from 2.4 ms to 7.6 ms and p50
from 1.11 to 1.26 ms — real, bounded, and still inside K1's 8 ms p50 and
16.7 ms p99 thresholds.

Against K6's threshold the wording matters: the criterion is "any frame > 33 ms
**attributable to the boundary**". These two are not — the boundary is 0.8% of
the work in the interval that produced them, and the same phase's own frame cost
never exceeded 13.02 ms. Reported as **PASS on the criterion as written, with a
measured caveat that the product does drop ~0.3% of frames while a large result
streams under an active scroll**, which is a real user-visible effect and
belongs on someone's list even though it is not ADR-0003's.

## K7 — build integration

Taken from CI (`gh run list --workflow ui.yml`, `gh run view <id> --json jobs`)
across the merged PRs #16–#19, against the 25-minute budget for a cold job.

| Run | Branch | qt-build windows | qt-build ubuntu | qt-build macos | qt-asan ubuntu |
| --- | --- | --- | --- | --- | --- |
| 35492858488 (**cold**, first ui.yml run, no caches) | M1.4 (#16) | 3.0 min ✅ | 2.1 min ✅ | 1.6 min ❌ **failed** | — |
| 35494036735 | M1.4 (#16) | 2.5 min ✅ | 1.4 min ✅ | 0.9 min ✅ | — |
| 35501931230 | M1.4 (#16) | 2.4 min ✅ | 1.5 min ✅ | 0.8 min ✅ | — |
| 35505514159 | ABI 3 (#17) | 2.4 min ✅ | 1.3 min ✅ | 0.8 min ✅ | — |
| 35507497118 | M1.6 (#18) | 3.7 min ✅ | 2.2 min ✅ | 1.2 min ✅ | — |
| 35508957508 | K5 ASan (#19) | 4.0 min ✅ | 1.9 min ✅ | 1.2 min ✅ | **2 min 21 s ✅** |

**Verdict: PASS**, and the literal clause is stated before the verdict rather
than after it. K7 says the criterion fails "if it fails on any of the three".
**It did fail on one of the three, once**: the very first `ui.yml` run, cold,
failed on macOS. Every run since — fifteen `qt-build` jobs across five later
runs, plus one `qt-asan` job, and the current `main` — has been green on all
three runners, and the cause was found
and fixed inside M1.4 (detail below). The PASS is therefore "green on all three
now, with one historical failure named and fixed", not "never failed".

The worst `qt-build` job observed is **4.0 min**, six times inside the
25-minute budget, and the cold run — the one without caches — was the *fastest*
Windows job of the set at 3.0 min, so caching is not what keeps it under budget.

The `qt-asan` job added in #19 is a fourth job on the same workflow, not an
extension of an existing one, and at 2 min 21 s it does not move K7's answer:
the longest single job on the workflow is still `qt-build (windows-latest)` at
4.0 min. Running the adapter suite under ASan costs roughly 2× the plain ubuntu
`qt-build` (2.35 min against 1.9 min in the same run), which is cheap for what
it buys (see K5).

The first-run macOS failure is recorded rather than dropped: it was
`Error copying file (if different) ... libreldex_ffi.dylib ... No such file or
directory` under ninja parallelism, the race that `ui/cmake/CopyIfDifferentRetry.cmake`
was added to fix inside M1.4. The same run also had a Windows `ffi-smoke`
configure failure. Both were fixed in that milestone and every run since has
been green on all three runners, including the current `main`.

## Sweeps — fetch size and fetches in flight (information, not criteria)

These feed the owner's pending "default fetch batch size" decision
(`SPEC.md` §12, still open after S14 found throughput is not monotonic in batch
size). **This recommends; it does not decide.**

### Headless, 1,000,000 rows, all 16 combinations, **n = 1 per cell**

Every cell below is a single run. That is stated here rather than implied,
because the repeats in the next subsection show how much a single run is worth.

| fetch rows | in flight | stream | RSS B/row | private B/row | batches | applyBatch p50 / p99 / max |
| --- | --- | --- | --- | --- | --- | --- |
| 100 | 1 | 713 ms | 137.1 | 140.1 | 10,000 | 0.6 / 2.6 / 76.6 µs |
| 100 | 2 | 521 ms | 137.0 | 139.8 | 10,000 | 0.6 / 2.6 / 84.7 µs |
| 100 | 4 | 521 ms | 137.2 | 140.2 | 10,000 | 0.6 / 2.6 / 95.1 µs |
| 100 | 8 | 514 ms | 137.2 | 140.1 | 10,000 | 0.5 / 2.5 / 73.2 µs |
| **1000** | 1 | 484 ms | 116.7 | 118.6 | 1,000 | 0.8 / 2.8 / 16.1 µs |
| **1000** | **2** | **472 ms** | **116.8** | **118.9** | 1,000 | 0.9 / 2.9 / 17.0 µs |
| **1000** | 4 | **464 ms** | **114.1** | **115.3** | 1,000 | 0.9 / 2.7 / 17.0 µs |
| **1000** | 8 | 473 ms | 115.8 | 117.2 | 1,000 | 0.9 / 2.9 / 18.3 µs |
| 10000 | 1 | 484 ms | 124.0 | 146.2 | 100 | 2.6 / 26.1 / 26.1 µs |
| 10000 | 2 | 474 ms | 123.2 | 146.1 | 100 | 2.1 / 18.5 / 18.5 µs |
| 10000 | 4 | 469 ms | 124.3 | 146.3 | 100 | 2.4 / 19.1 / 19.1 µs |
| 10000 | 8 | 464 ms | 125.5 | 147.4 | 100 | 2.7 / 17.0 / 17.0 µs |
| 50000 | 1 | 579 ms | 115.1 | 127.7 | 20 | 7.4 / 25.9 / 25.9 µs |
| 50000 | 2 | 579 ms | 114.8 | 127.0 | 20 | 7.8 / 25.8 / 25.8 µs |
| 50000 | 4 | 590 ms | 114.1 | 126.7 | 20 | 7.4 / 25.1 / 25.1 µs |
| 50000 | 8 | 593 ms | 114.6 | 127.7 | 20 | 7.1 / 23.3 / 23.3 µs |

### The four 1,000-row cells, repeated (round 2, n = 3 more each)

Round 1 said the four in-flight settings at 1,000 rows/fetch were "within
noise" from one run each, and called 1000/4 "marginally better". Three repeats
of each cell turn that into a measurement — and change the conclusion slightly:

| in flight | round 1 (n=1) | round 2 repeats (n=3) | round-2 median | round-2 spread |
| --- | --- | --- | --- | --- |
| 1 | 484 ms | 585 / 620 / 580 ms | 585 ms | 40 ms |
| 2 | 472 ms | 573 / 605 / 577 ms | 577 ms | 32 ms |
| 4 | 464 ms | 579 / 571 / 563 ms | **571 ms** | 16 ms |
| 8 | 473 ms | 576 / 570 / 595 ms | 576 ms | 25 ms |

**The between-session shift is ~100 ms and the between-cell difference is
≤ 14 ms.** Round 2's whole set is about 100 ms slower than round 1's, on the
same binary and the same machine; within round 2 the four settings differ by
less than half of any single cell's own spread. "Within noise" is now a
measurement rather than an impression, and the round-1 claim that 4 in flight
was "marginally better" than 2 is **not supported** — the 6 ms between their
medians is a quarter of the spread inside either one.

### In-app, one run per size, **n = 1 per cell**, 2 fetches in flight, `full` sweep

| fetch rows | execute → rows | execute → first frame | `full` p50 / p99 / max | RSS B/row | applyBatch p50 / p99 |
| --- | --- | --- | --- | --- | --- |
| 100 | 1.46 ms | 434.21 ms | 7.61 / 9.78 / 12.82 ms | 196.7 | 1.6 / 12.2 µs |
| 1000 | 1.92 ms | 435.26 ms | 7.87 / 10.59 / 15.01 ms | 172.6 | 2.0 / 15.9 µs |
| 10000 | 6.91 ms | 8.77 ms | 7.89 / 11.24 / 13.82 ms | 185.2 | 12.6 / 29.3 µs |
| 50000 | **30.64 ms** | 459.37 ms | 7.76 / 10.22 / 12.77 ms | 173.8 | 27.9 / 46.4 µs |

(The `execute → first frame` column is a single sample per size and carries the
once-per-process ~432 ms of K2; the 8.77 ms at 10,000 is one run that happened
to paint before that cost was paid, not a property of that batch size. The
column is kept because dropping an inconvenient sample is worse than labelling
it.)

### What the sweep says

- **Scroll frame time is flat in fetch size** (p99 9.8–11.2 ms across a 500×
  range). The model's 1,024-row format window is what decouples them, and it
  works: K1 does not depend on this decision.
- **100 rows/fetch costs memory** — 137 B/row against 114–117, i.e. ~20 MB more
  per million rows in per-batch overhead — and is the slowest to stream at low
  concurrency (713 ms at 1 in flight).
- **50,000 rows/fetch costs first-row latency**: 30.64 ms to the first row
  against 1.5–1.9 ms, because nothing can be shown until the whole batch exists.
  It is also the slowest stream (579–593 ms).
- **10,000 rows/fetch is not faster than 1,000** and costs ~28 B/row more in
  private bytes, echoing S14's finding that bigger is not better.
- **More than 2 fetches in flight buys nothing**, now with repeats behind it
  (n = 3 per cell at 1,000 rows/fetch): the four settings' medians span 14 ms
  while each cell's own spread is 16–40 ms. 1 in flight is measurably worse at
  small batch sizes (713 → 521 ms at 100 rows/fetch) but not at 1,000, where it
  is 8–14 ms behind the others and inside the noise.

**Recommendation, for the owner to decide:** default **1,000 rows per fetch with
2 fetches in flight**. It is the fastest or tied-fastest stream, the lowest
per-row memory, the lowest per-batch boundary cost among the sizes that stream
well, and the second-best first-row latency. It should remain a user setting
with a per-profile override and a bounded range, per `SPEC.md` §12 — and it
should be re-measured on a real network in M5.7, because every number above has
zero network latency in it.

## What went wrong while measuring, and what was changed because of it

Recorded because a measurement report that hides its own corrections is not
evidence.

1. **The first `flick` pattern did not move the view at all.** Calling
   `Flickable::flick()` from `afterAnimating` — inside the polish-and-sync
   cycle — left `contentY` at 0 while producing beautiful frame times (p50
   5.35 ms, scene-graph work 0.037 ms: a static scene). It was caught by adding
   the `contentY` each phase actually reached to the output, and the pattern was
   replaced with an explicit flick-shaped velocity profile. **Every phase now
   reports `endContentY` and `rowsTravelled`, and those columns are in the
   tables above** so the reader can check that motion happened.
2. **`beforeFrameBegin`/`afterFrameEnd` is the wrong pair for per-frame CPU
   work.** `QRhi::beginFrame`'s frame-latency wait runs inside it, so it
   reported 252 ms per frame for a frame whose work was 0.3 ms. Changed to
   `beforeSynchronizing`/`afterRendering`, which brackets the scene graph's own
   CPU work and excludes both the wait and the present.
3. **A single-run configuration recorded 85 runs.** `firstFrameAfterInsert`
   re-arms on every batch, so the K2 accounting kept appending entries with no
   execute of their own. Fixed; and the warm re-execute is now submitted
   synchronously, because a zero-millisecond timer let a batch land between
   clearing the marks and recording the next execute, producing negative
   latencies.
4. **The K3 baseline was initially taken before the window had drawn**, which
   charged ~28 MB of scene-graph and driver initialization to the rows and read
   as +200.7 MB. The driver now takes a second baseline on an idle,
   already-drawn app and reports both. (Both readings are now in the K3 table,
   because the second one is *not* obviously wrong — see K3.)

The next three were found by an **independent review of round 1 of this
report**, not by me, and two of them invalidated claims the report was making.

5. **The "5.31 ms presentation floor" was the driver's own pacing timer, and
   the conclusion drawn from it was wrong in the flattering direction.** It is
   `QT_QPA_UPDATE_IDLE_TIME` (5 ms by default on Windows), paid once per frame
   because `ScrollDriver` paces with `requestUpdate()`. Round 1 called it a
   property of the machine's present path and *subtracted* it from the measured
   p50 to claim "about 2.5 ms of application work". Both halves were wrong: the
   wait overlaps the work rather than adding to it, so the subtraction is
   invalid, and the measured application cost with the timer disabled is
   **5.6 ms**, not 2.5 ms — the round-1 statement understated it by 2.2×. The
   affected rows are marked below; K1 is re-answered from passes with the timer
   disabled.
6. **The GUI thread's per-frame cost was not measured at all** in round 1 — and
   it is where `TableView`'s polish, delegate reuse and every `data()` call
   live. `Metrics` gained three more brackets (§K1) so the frame can be stated
   as a sum of measured parts.
7. **The fix for one first-frame bug introduced another, and the instrument
   caught it.** K2's end point is latched on the first `frameSwapped` after the
   rows were inserted, which with the threaded render loop can be a frame whose
   *sync* happened before the insert — i.e. K2 could read one frame early. The
   fix waits for an `afterSynchronizing` after the insert. The first version of
   that fix re-armed the gate on **every** batch, not just the first, so during
   a 1,000,000-row stream the latch did not close until the stream was nearly
   over: a first frame that arrives in ~11 ms was reported as **1,401 ms**. It
   was caught by reading a number that was absurd rather than merely large.
   Guarded to the run's first insert; K2 re-measured afterwards.

No change was made to `crates/**` or to `reldex.h`. The adapter changes are
listed in "What this task added".

## Limitations — what was NOT measured

- ~~**Vsync-on frame cadence.**~~ **Closed in round 2.** It was the largest gap
  in round 1 and it is now measured: 36,008 frames at 59.978 Hz across two
  scales and two frame budgets, with an idle control in every run. See K1.
- **What the 122.70 ms GUI-thread frame was.** One frame in 36,008; located on
  the GUI thread by bracket, not attributed to a cause. A sampling profiler over
  the GUI thread would close it; this task did not run one.
- **Two sessions on one hub, one of them blocked.** K6's "or any other session"
  clause is measured across *hubs* (n = 15 per side, see K6) because the M1.6
  adapter puts exactly one session in each `Bridge`/hub. The same-hub case —
  where a shared event queue and waker could actually couple two sessions — is
  **NOT MEASURED** and is not reachable here at all. It belongs with M3's
  multi-session work and should be measured there before anyone claims K6
  covers it.
- **Whether `applyBatch`'s tail matters.** The worst single batch in ~5,000
  in-app batches was 257.9 µs with vsync on and 791.4 µs with an unthrottled
  render loop, both above K4's 200 µs. The distribution is in K4; whether a
  criterion on the maximum rather than on p99 is the right one is an owner
  question this report does not answer.
- **True system-DPI 150%.** `QT_SCALE_FACTOR=1.5` emulates it; the owner's
  display scale was not changed. Differences that only appear with a real
  per-monitor DPI change (mixed-DPI moves between the two monitors, native
  non-integer scaling paths) are untested.
- **Linux and macOS frame time, memory and latency.** CI builds and runs the
  offscreen correctness half on both; no GPU-backed frame time exists for
  either, and ADR-0003 D10 item 4 says offscreen timings are not representative.
- **A real database.** Everything here is the mock driver, in-process, with zero
  network latency and a generator that never blocks on I/O. M5.7 owns the re-run
  against the real database; until then no throughput number should be quoted as
  a product number.
- **AddressSanitizer on Windows** (K5) — not installable on this machine without
  a Visual Studio component change. Linux CI covers ASan/UBSan/LSan over the
  whole adapter suite and is clean, but MSVC's ASan would exercise a different
  allocator and a different threading runtime, so "no sanitizer finding on
  Linux" is not the same claim as "no sanitizer finding on the platform this
  spike was measured on".
- **Mobile.** Nothing on Android or iOS was built, run or measured here.
- **LOB columns**, which do not cross the boundary yet (ADR-0003 A7), and
  therefore contribute nothing to any number above.
- **Multi-session scrolling on one session's result** (see K6): not
  representable in the M1.6 adapter.
- **Whether the cross-adapter present costs anything.** Qt's D3D11 backend
  selects the NVIDIA discrete GPU while the measured window's display is driven
  by the AMD integrated GPU. One exploratory run with `QT_D3D_ADAPTER_INDEX=1`
  (600 frames/phase, production settings, **n = 1**) produced `full` p50
  **5.022 ms** against 5.615 ms on the default adapter and render work 1.527 ms
  against 1.800 ms — consistent in direction with a cross-adapter present
  costing something, but one run is not a result and this is not claimed as one.
- **Miri** (ADR-0003 A9) — still not run; unchanged by this task.

## Recommendation for M1.9

**Accept ADR-0003, subject to four things being decided or recorded — not
resolved by this report.** This is advice; the lead and the owner decide.

An independent review of round 1 recommended "accept with conditions: K1
evidence rewritten, owner ruling on K2 recorded in the ADR, K3 conservative
reading disclosed, K6 other-session clause measured or recorded as not
measured". **I agree with all four**, they are all done or disclosed above, and
round 2 adds two the review did not ask for: K1's p99 clause needs the same kind
of ruling K2's does, and scrolling while a result streams is the one
configuration that drops frames above the machine's own rate.

The reasoning:

- **Every criterion whose subject is the boundary passes, most by a wide
  margin, and the boundary's share is now measured rather than inferred.** A
  batch crossing is 0.9 µs headless against a 200 µs budget; the boundary's
  share of a whole drain is **0.4–2.0 µs, under 1%** of the 80–250 µs the UI
  thread actually spends; a warm cell is 100–120 ns against 200 ns; a million
  rows cost 114–119 B/row headless against 200 B/row; teardown under a 10,000-
  iteration flood leaks nothing and hangs never, with a clean ASan/UBSan/LSan
  run over the whole adapter suite on Linux CI behind it; a session blocked for
  a whole phase changes another session's 867 ms stream by **0.0%** and costs
  the UI no frame at all; the build is inside a sixth of its CI budget on all
  three runners.
- **Both non-green results are located outside the boundary, specifically.**
  K2's cold number is now apportioned from Qt's own instrumentation: ~250 ms of
  D3D11 device creation plus one 551 ms `polishItems` pass that is delegate
  instantiation (identical at 1,000 and 1,000,000 rows, absent at 1 row, 7–10 ms
  on the second execute). K7's single macOS failure was a build-script race
  already fixed in M1.4.
- **K1's margin is real but smaller than round 1 claimed.** The honest number is
  5.0–5.7 ms of frame cost against an 8 ms threshold — about 30% of headroom,
  not the 3× the withdrawn subtraction implied.

The four things to decide or record:

1. **Rule on what K1's `p99 > 16.7 ms` names.** Measured literally on a 60 Hz
   vsync-on swap distribution it fails for a window doing nothing, so it cannot
   mean that. On the dropped-frame reading it passes (0.022%, at the idle
   control's own rate). Whichever the owner intends should go into the ADR's
   evidence section with the number that answers it.
2. **Rule on whether K2 is about the warm path.** As written it has no
   qualifier and the cold process reading — 903.55 ms with vsync on — fails by
   6×. The warm path passes by ~9× and the cold cost is a Qt startup cost with a
   named composition. Either the criterion gains a qualifier or K2 is recorded
   as failed-with-cause; this report does not choose.
3. **Record K3's conservative reading.** The rows cost 172.7 MB; the whole
   process from start costs 200.3–200.8 MB of working set, which crosses
   200 × 10⁶ B and does not cross 200 MiB. The ADR should say which baseline and
   which of working set / private bytes it means.
4. **Record what is still NOT MEASURED**, chiefly two sessions on one hub with
   one blocked (not reachable in this adapter; belongs to M3) and the cause of
   the single 122.70 ms GUI-thread frame.

And two items that are not ADR-0003's but should not be lost:

- **The ~800 ms cold first paint** (≈250 ms D3D11 device + ≈551 ms delegate
  polish) is the first thing a user sees on every launch and sits against
  `SPEC.md` §19's "warm desktop startup < 1 second desirable". Fix candidates
  are named in K2; none was implemented here.
- **Scrolling a result that is still streaming drops ~0.3% of frames**
  (2 in 601, worst 49.53 ms, GUI-thread bound). It is not attributable to the
  boundary — which is 0.8% of the work in that interval — but it is a real
  user-visible effect and it was invisible to round 1.

Re-opening ADR-0003 is not warranted on this evidence. The hand-written C ABI
did what D4 claimed for it: text crosses zero-copy, the bulk formatter keeps a
warm cell at ~100 ns, one call per column per batch keeps the per-batch cost
three orders of magnitude inside its budget, and in 36,008 vsync-on frames the
cache-hostile patterns were statistically indistinguishable from an idle window.

## Reproducing

Exact commands are in `ui/README.md`, "Reproducing the M1.8 numbers". The
summarised data behind every table is in
`docs/exec-plans/active/phase-1-s15-data/` (57 KB):

| File | What it holds |
| --- | --- |
| `k1-vsync-frames.csv` | round 2, vsync on: 20 phases × two scales × two frame budgets |
| `k1-production-frames.csv` | round 2, vsync off: frame production, including the `QT_QPA_UPDATE_IDLE_TIME` A/B |
| `k1-k6-frames.csv` | round 1, the pacing-limited pass, kept for comparison |
| `k2-latency.csv` | both rounds, all variants, with the round and vsync state per row |
| `k3-memory.csv` | both baselines × both metrics × headless and in-app |
| `k4-boundary.csv` | `applyBatch` and per-cell `data()` |
| `k4-drains.csv` | whole-drain and boundary-only cost, per run and per phase |
| `k6-frames.csv`, `k6-other-session.csv` | K6's phases and the 15-per-side other-session comparison |
| `sweep-headless.csv`, `sweep-in-app.csv` | the fetch-size sweep, now with `rep` for the repeated cells |
| `summarize.py` | reads a run's JSON and prints these tables (`--runs`, `--skip N`, `--gui`, `--drains`, `--stream`) |

Per-frame raw samples are deliberately **not** committed — 36,000 frames per
run is noise at this scale; `RELDEX_S15_CSV=<path>` re-emits them for anyone who
wants to check a percentile.

## What this task added

**No behaviour was changed to improve a number.** ADR-0003 allows an
adapter-side fix when a criterion fails with an obvious adapter-side cause, and
none did: the two non-green results are located outside the adapter (K2's Qt
first-frame cost, K7's fixed build-script race), so nothing in `ui/adapter` or
`ui/app` was altered except to measure. Every correction listed under "What went
wrong" is a correction to the *instrument*, and the before/after numbers for
each are given there. Nothing in `crates/**` or `reldex.h` was touched.

Measurement code only, all off by default:

- `ui/adapter/ScrollDriver.{h,cpp}` — new. The env-gated scroll/measurement
  driver: motion patterns, the K2 run loop, the K6 block phases, the memory
  baselines, and the JSON summary. Inert unless `RELDEX_S15_SCROLL` or
  `RELDEX_S15_RUNS` is set. **Round 2** added the `streambase`,
  `streamblocked` and `streamscroll` phases (K6's other-session clause and
  scrolling under a live stream), a third `Bridge` for the working session, and
  per-phase reporting of the GUI/handover/sync brackets and of the drains that
  happened during that phase.
- `ui/adapter/Metrics.{h,cpp}` — `applyBatch` samples, scene-graph work samples,
  percentile/threshold-count aggregates, per-run K2 marks, a
  `firstFrameAfterInsert` signal, and a render-loop-thread check. **Round 2**
  added three more per-frame brackets (`afterAnimating`, `afterSynchronizing`
  handlers), the per-drain boundary split, `clearDrains()`, and the corrected
  first-frame latch. All behind the existing `RELDEX_UI_METRICS` flag; every
  recording entry point is still one relaxed atomic load when it is off.
- `ui/adapter/Bridge.{h,cpp}` — owns the driver, and defers `autoStart()` to it
  when a measurement run wants a clean memory baseline. **Round 2** times the
  boundary's share of each drain (two extra clock reads per event, behind the
  same flag).
- `ui/adapter/SessionController.cpp` — times `applyBatch` (K4) behind the
  metrics flag; `RELDEX_S15_AUTOFETCH`.
- `ui/app/main.cpp` — `RELDEX_S15_NO_VSYNC` sets the default swap interval to 0;
  **round 2** added `RELDEX_S15_LOG`, a file sink for Qt's own logging
  categories, because this is a GUI-subsystem binary with no console. Installed
  only when the variable is set.
- `ui/app/Main.qml` — one line attaching the driver.
- `ui/tests/tst_resultmodel.cpp` — `spikeS15BoundaryCost`, skipped unless
  `RELDEX_S15_K4=1`, **with no timing assertion of any kind**: it measures and
  reports, and cannot fail on a number. No timing bound was added anywhere in
  the automated suite because of this work, in either round.
- `docs/exec-plans/active/phase-1-s15-data/summarize.py` — `--gui`, `--drains`,
  `--stream` and `--skip N` (which prints how many runs it dropped).
- `ui/README.md`, and this report.

**Merge note for M1.9.** Round 1 was committed as `2743d93` on
`phase-1/m1-8-s15` over base `daa8d67`; `main` was then merged in at `5b50cbf`
(bringing PR #19, the ASan CI job). Round 2's changes are **uncommitted** on top
of `5b50cbf`. PR #19 touched `ui/CMakeLists.txt`, `ui/build.sh`,
`.github/workflows/ui.yml`, `ui/cmake/Sanitizers.cmake`, `ui/tests/lsan.supp`,
`phase-1-toolchain.md` and `ui/README.md`. **This task edited none of those
except `ui/README.md`**, and the two README edits do not overlap: #19's is the
AddressSanitizer section, this task's is the instrumentation section above it.

## M6.9 — cold first paint follow-up (2026-09-25)

M1's exit-gate item named "the ~800 ms cold first paint" as something not
ADR-0003's but not to be lost. M6.9 investigates the fix candidates this
report named and either implements the ones proven to help or reports why
not. **Result: no code change.** Every low-risk candidate tested gave no
material gain over a *fresh* baseline (itself lower than this report's
903.55 ms — see below, and see why before reading anything else here as "cold
first paint got fixed"), and the two candidates with real potential each
require either a genuine behaviour change or more than a minimal adapter
change, so both are recorded as a recommendation for M4.x rather than
implemented against today's harness. Measured on the same machine as the rest
of this report (`AMD Ryzen 7 5700G` / dual-GPU, Windows 11, build 26200),
branch `phase-1/m6-9-cold-first-paint` over `origin/main` at `1fddf9a`,
`bash ui/build.sh --release` (Release, unchanged CMake config), `RELDEX_UI_METRICS=1
RELDEX_S15_AUTORUN=1 RELDEX_S15_ROWS=1000000 RELDEX_S15_RUNS=1
RELDEX_S15_START_DELAY_MS=0` per cold run — the exact K2 method above, against
`Harness.qml` (unchanged; `Main.qml`, M3.1's app shell, has no `TableView` yet
— see "Does the baseline still mean what K2 measured" below).

**Machine state, checked before every batch, per this task's own
constraint.** Two other workers ran `cargo`/`rustc` builds on this machine
throughout. Before each batch, `tasklist` was checked for
`cargo`/`rustc`/`cl`/`link`/`cmake`/`ninja`/`clang`, and the batch waited
(polling every 15 s, up to 15 min) until none were running; one batch found
the machine busy again between the check and the first launch and re-waited
before recording anything. Every number below was captured with zero matching
processes running at launch time. Display: awake and in interactive use by
other sessions throughout (consistent with this report's "round 2"; the
`DwmFlush` probe itself was not re-run — see Limitations). No system, display
or power setting was changed; no synthetic input was sent.

### Does the baseline still mean what K2 measured

`Harness.qml` is byte-for-byte what M1.8 measured (`git diff` against
`5b50cbf`'s copy is empty apart from path). M3.1 (merged 2026-09-25, the same
day as this task) replaced `Main.qml` — the app's default window — with the
real shell (`Sidebar`/`WorksheetArea`/`OutputPanes`/`StatusBar`); its result
pane is a `Text` placeholder (`WorksheetArea.qml`), not a `TableView`. So:

- The 903.55 ms / 15.85 ms numbers this task reproduces are still about
  `Harness.qml`'s `TableView`, exactly as K2 measured them — M3.1 did not
  touch that file.
- **They are no longer about what a user sees by default.** Launching
  `Reldex.exe` without `--harness` today pays the ~185–191 ms D3D11
  device-creation cost (any `QQuickWindow` does), but not the ~370–550 ms
  delegate-instantiation polish, because there is no `TableView` to
  instantiate — M4.x's real result grid is what will reintroduce it, and
  should read this section before it does.

### Fresh baseline vs the S15 number — the biggest single finding

| | n | min | median | max | mean | CoV |
| --- | --- | --- | --- | --- | --- | --- |
| S15 (2026-09-20), cold, execute→first frame | 30 | — | 903.55 | 1,551.91 (p95 979.23) | — | — |
| **M6.9 fresh baseline (2026-09-25), same scenario** | **12** | **389.56** | **576.19** | **898.95** | 567.05 | **23.1%** |
| M6.9 fresh baseline, execute→rows inserted | 12 | 2.06 | 186.00 | 350.78 | 158.15 | 63.4% |

**The baseline moved by itself, with zero code change, before any candidate
was tried.** `Harness.qml`, `Bridge`, `SessionController` and `Metrics` are
identical to what S15 measured; only the calendar date, five days of this
machine's use, and this run's own five-to-twelve-sample noise separate these
two rows. The median dropped ~36% (903.55 → 576.19 ms) and the highest single
sample (898.95 ms) is close to S15's *median*. The likely cause is D3D11
shader/pipeline and OS file-cache state warmed by everyday use of this
machine since 2026-09-20 (S15 itself named DXGI adapter enumeration and
driver-side caching as machine state, not application state) — nothing in
`crates/**` or `ui/**` changed the number. **This is why every candidate
below is judged against 576.19 ms, not 903.55 ms**, per this task's own
instruction, and it is also why a reader must not credit M6.9 with a "36%
win" — that movement happened before any candidate was tested.

Coefficient of variation is reported because it matters here: 23.1% on the
metric K2 is judged on, and 63.4% on the intermediate "rows inserted" mark,
which is consistent with the diagnosis below (rows-inserted races the D3D11
blocking sync, so its own latency is close to bimodal).

### Breakdown, reproduced fresh (Qt's own scenegraph timing)

Same method as S15 ("K2 — what the cold first paint is made of"),
`QT_LOGGING_RULES="qt.scenegraph.time.*=true;qt.scenegraph.general=true;qt.rhi.general=true"`
through `RELDEX_S15_LOG` (`Reldex.exe` is GUI-subsystem, no console), one run
per row count:

| Rows | first-execute frame `polish` | first-execute frame `blockedForSync` | second frame `polish` / `blockedForSync` |
| --- | --- | --- | --- |
| 1 | 0 ms | 191 ms | 0 / 1 ms |
| 1,000 | 366 ms | 188 ms | — |
| 1,000,000 | 372 ms | 185 ms | — |

Same shape as S15 (identical at 1,000/1,000,000 rows, absent at 1 row — the
first screenful of delegates, not the first row), smaller in absolute terms
(185–191 ms D3D11 vs S15's 249–271 ms; 366–372 ms polish vs S15's 551–579 ms)
— consistent with the same machine-state drift the baseline shows. `185 to
191 + 366 to 372 ≈ 551–563 ms`, which lands inside this session's 389.56–898.95 ms
range and close to its 576.19 ms median: **the two named costs still account
for essentially the whole number**, same conclusion as S15, fresh evidence.

`QT_DEBUG_PLUGINS=1` (captured through the same log sink) produced 48
plugin-factory-loader lines for platform-plugin discovery at startup — a
short, fixed cost with no visible per-run timing blowup, and no room for it
to be large given the arithmetic above. **DLL/plugin loading is not what
dominates the cold number, confirmed fresh.**

Fresh warm-path check (`RELDEX_S15_RUNS=31`, run 1 dropped, n=30): execute→first
frame **p50 16.38 ms**, p95 16.61, max 16.62 — S15 measured 15.85 ms. Within
noise of one vsync period; **the warm path is unaffected**, as it must be
since nothing changed.

### Candidates measured

| Candidate | Category | n | median (first frame) | CoV | vs 576.19 ms baseline | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| `QSG_RENDER_LOOP=basic` | D3D11 share | 8 | 594.78 ms | 4.1% | +18.6 ms (noise) | No material gain |
| `QQuickWindow::setGraphicsApi` explicit | D3D11 share | — | — | — | — | Not run: S15's own environment table already shows D3D11 selected without an override; an explicit call would be confirming, not changing, the default |
| Pre-creating the window earlier (a hidden warm-up `QQuickWindow` ahead of `engine.loadFromModule`) | D3D11 share | — | — | — | — | Not run: Qt Quick's RHI/D3D11 device is per-window; sharing it with the real window needs `QQuickGraphicsConfiguration::setDevice()`-style explicit resource sharing, which is materially more than a minimal change and was not attempted under this task's time budget |
| `Loader { asynchronous: true }` around the `TableView` | delegate share | — | — | — | — | Investigated, not run (see below) |
| Pre-warm the delegate with a throwaway small query before the real one | delegate share | — | — | — | — | Investigated, not run — real behaviour change, see below |

**`QSG_RENDER_LOOP=basic`, in full**: median first-frame 594.78 ms (n=8, min
563.16, max 641.15) against the 576.19 ms fresh baseline — a difference well
inside both samples' own noise, so **no material gain**. Its one real effect
is variance: CoV drops from 23.1% (baseline, n=12) to 4.1% (n=8) — a far more
predictable number, not a faster one. That is not nothing (a single-threaded
render loop removes a GUI/render-thread handoff this report's K1 section
already measures), but changing the render loop is a decision with a blast
radius across K1's own numbers (frame pacing, GUI/render-thread split) that
this task did not re-verify, so it is reported and not adopted for a tie on
the metric this task is scored on.

**`Loader.asynchronous`, why it was not implemented.** The candidate spreads
delegate creation over frames instead of one 551 ms `polishItems` pass — but
`ScrollDriver::attach(window, flickable)` (`Harness.qml`'s
`Component.onCompleted`) needs the real `TableView` item to exist *before* the
measured run starts, and an async `Loader`'s item does not exist until
incubation completes. Wiring this correctly means deferring
`scrollDriver.attach()` and the run's own start until `Loader.status ===
Loader.Ready` — more than a minimal change — and it introduces a real risk to
what K2 measures: `Metrics::firstFrameAfterInsert` only requires *a* frame to
swap after the model has rows, not that the `TableView` exists yet. An async
`Loader` could report a better K2 number by swapping an empty frame while the
grid is still incubating, which would be measuring the harness's own
deferral, not a user seeing their result sooner. This is not a reason to
discard the idea — `QQmlIncubationController` tuning belongs with it — but it
needs a real result grid (M4.x) and a metric that can tell "empty frame" from
"populated frame" apart, neither of which exists yet.

**Pre-warming the delegate, why it was not implemented.** This is the
candidate with the largest measured ceiling: the fresh warm-path number
(16.38 ms) *is* what a pre-warmed second execute already costs, per this same
report's own K2 method — S15 found the delegate polish "drops to 7–10 ms on
the second execute in the same process," and this task's fresh warm-path scan
confirms the shape still holds. If a throwaway small query ran once before
the user's first real one, the real one's cold number should approach
D3D11 (~185 ms) plus a warm execute (~16 ms) — roughly a third of today's
576 ms median. It was not implemented here because it is a genuine behaviour
change (an extra, transient execute against the model), `SessionController`'s
`mockRows` is already a writable property so the plumbing exists, but
`Bridge::run()` is shared with `tst_bridge.cpp` and, more importantly, there
is no real `TableView` for it to warm today (see "Does the baseline still
mean" above) — adding it now would be dead code exercised only by the
harness. **Recommended verbatim for M4.x**: when the real result grid lands,
execute a tiny (few-row) throwaway query against it during startup, before
the connection/worksheet UI is interactive, so the one-time delegate
instantiation is paid while the user is not waiting on a query.

### Recommendation for the owner's K2 ruling

Unchanged in substance from S15's own recommendation, now with two additional
facts: the cold number moved ~36% between 2026-09-20 and 2026-09-25 with zero
application-code change (machine/driver state, not this task, not ADR-0003),
and part of the *remaining* cold cost is itself an artifact of the harness's
own "execute from `Component.onCompleted`" pattern — a human pressing
"Execute" after looking at a rendered window would not contend with D3D11
device creation the way this synthetic cold-start does. The delegate-polish
share (366–551 ms across both measurement sessions) is the part a real user
would still pay on their first query, and it has a named, measured,
not-yet-applicable fix (pre-warm the delegate) waiting for M4.x's result
grid. Recommend the owner still rule K2 as the warm path (passes by ~9–55×
depending on session); record the cold number as a known, diagnosed, Qt-side
one-time cost with a concrete mitigation deferred to M4.x, not as a boundary
failure.

### Limitations (M6.9)

- **The `DwmFlush` compositor probe was not re-run.** S15's round 1 vs round 2
  showed this matters (throttled vs live compositor). This task's numbers
  were taken on a machine in continuous interactive use by other workers
  throughout (strong indirect evidence the compositor was live, not
  throttled), but the direct probe from S15's method was not repeated.
- **Only one candidate reached a full ≥5-run batch** (`QSG_RENDER_LOOP=basic`).
  `QQuickWindow::setGraphicsApi` and the window-pre-creation idea were reasoned
  about, not measured, because the former is confirmed already-default from
  S15's own environment table and the latter needed more implementation than
  this task's minimal-change bar allows; both are named rather than silently
  dropped.
- **The delegate-instantiation candidates (`Loader.asynchronous`, the
  pre-warm query) were reasoned and partially evidenced (the warm-path number
  is the pre-warm candidate's ceiling) but not built and measured as code**,
  because the current app shell has no `TableView` for either to act on — see
  "Does the baseline still mean what K2 measured".
- **CoV on "rows inserted" (63.4%) is high enough that its median (186.00 ms)
  should be read as "contended with D3D11 device creation," not as a stable
  per-run cost** — consistent with, not a correction to, S15's own framing of
  this column as "contention, not work."
