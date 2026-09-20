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

| # | Criterion | Threshold | Measured | Verdict |
| --- | --- | --- | --- | --- |
| K1 | Scroll frame time over the full 1M rows, 100% and 150% | p99 > 16.7 ms, or p50 > 8 ms | Worst pattern (whole result traversed): **p50 7.75 ms, p99 10.31 ms, max 14.56 ms** at 100%; **7.69 / 10.19 / 12.40 ms** at 150%. Of that p50, **5.31 ms is a fixed presentation floor this machine imposes on an idle window**; scene-graph work per frame is p50 1.00 ms, p99 1.86 ms | **PASS**, with a caveat: the vsync-on swap cadence was **not measurable** (see K1) |
| K2 | Execute → first row painted, mock driver, in-process | > 150 ms | Warm (2nd..31st execute, n=30): **1.30 ms to rows, 11.16 ms to the painted frame**. Cold process: **190.65 ms to rows, 621.97 ms to the painted frame** (n=30) | **PASS warm / FAIL cold**, cause diagnosed **outside** the boundary |
| K3 | Retained memory, 1M rows of the S14 shape | > 200 MB RSS growth | Headless **+114.1…118.9 MB RSS** (114–119 B/row), private +114.9…121.7 MB, n=3. In-app **+172.7 MB RSS** (172.7 B/row), private +142.1 MB, n=3 | **PASS** |
| K4 | Event delivered → batch described and columns viewable; per-cell `data()` | > 200 µs; or per-cell avg > 200 ns | `applyBatch` headless **p50 0.90 µs, p99 2.9 µs, max 25.8 µs** over 1,000 batches; in-app **p50 2.6–2.9 µs, p99 16.2–17.5 µs, max 170.6 µs**. Warm per-cell: **NUMBER 102.6–114.6 ns, VARCHAR2 99.7–107.7 ns, DATE 112.4–120.4 ns** | **PASS** |
| K5 | Waker teardown, 10,000 iterations, ASan | any use-after-free, race, or hang | Here, 3 runs: **10,000 iterations in 6.30 / 6.35 / 6.70 s**, no hang, no crash, `reldex_live_counts` back to baseline every time. Under **ASan+UBSan+LSan on Linux CI**: 10,000 iterations in 13.13 s, **zero sanitizer reports**, live counts back to baseline | **PASS**, with ASan evidence from Linux CI (not from this machine) |
| K6 | UI never stalls under a 10 s-class blocking statement | any frame > 33 ms attributable to the boundary | Scrolling 1M rows with a second session blocked for the whole 16.2 s phase: **max frame 8.87 ms, 0 frames > 16.7 ms, 0 > 33 ms**, against a baseline sweep of max 7.95 ms | **PASS** |
| K7 | CMake+Corrosion+Qt builds and offscreen tests pass on all three runners | fails on any of the three, or a cold job over 25 min | Cold (first run, no caches): windows **3.0 min**, ubuntu **2.1 min**, macos **1.6 min but FAILED**. Warm: windows 2.4–4.0, ubuntu 1.3–2.2, macos 0.8–1.2 min; the new `qt-asan` ubuntu job 2 min 21 s. Current `main` green on all three | **PASS**, with the first-run macOS failure recorded |

**No criterion's failure is located in the boundary.** The two non-green
results are K2's cold-process reading, whose cost is one-time Qt scene-graph
work with the boundary contributing 2 ms of it, and K7's first-ever macOS run,
which was a build-script race fixed inside M1.4.

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
| Commit | `daa8d6718c8f0e1785c57c064548191ea2290016`, worktree branch `phase-1/m1-8-s15` (this task's changes uncommitted) |
| Window | 962×665 logical at 100%; 961×657 logical / 1.5× device pixels at 150% |

**Background load that could not be controlled, and that matters:** the machine
was otherwise idle, but the desktop had been idle long enough for the display
to power down, and **the Windows desktop compositor was therefore throttled to
~4 Hz for the whole session**. This is not an inference: `DwmFlush()` — which
blocks until the next desktop composition — returned with a **median of
251.55 ms** (min 250.28, max 265.46) when probed from a separate process. It
affects every application on the machine, not just this one, and it is the
single most important caveat in this report. See K1.

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

### The vsync problem, stated plainly

With vsync on, a swap interval quantizes to the refresh period: p50 ≈ 16.7 ms at
60 Hz means "on budget", so **K1's `p50 > 8 ms` clause cannot be evaluated from
swap intervals at all on a 60 Hz display**. That was expected and planned for.

What was not expected is that the vsync-on measurement is worthless here for a
second and larger reason. Measured at 1,000,000 rows, vsync on:

| Phase | frames | p50 | p99 | max | scene-graph work p50 |
| --- | --- | --- | --- | --- | --- |
| `idle` (nothing moving) | 60 | 251.80 ms | 266.45 ms | 266.45 ms | 0.084 ms |
| `full` (the entire result traversed) | 60 | 252.13 ms | 266.56 ms | 266.56 ms | 1.726 ms |

A window doing nothing and a window traversing a million rows swap at exactly
the same cadence, ~4 Hz, while their actual work differs by 20×. That is the
compositor's cadence (`DwmFlush` median 251.55 ms), not the application's.

Three things were tried and are recorded because they did **not** fix it:
`raise()` + `requestActivate()` (Windows refuses a foreground change from a
process that does not own the foreground); `Qt::WindowStaysOnTopHint` (the
window is then genuinely on top — and still throttled); and a process-local
`SetThreadExecutionState(ES_DISPLAY_REQUIRED)` (documented not to wake a display
that is already off). Waking the display would have needed synthetic input on
the owner's desktop, which was not available to this task. **No power or display
setting was changed to work around it.**

So K1 is answered from **frame production** instead: `RELDEX_S15_NO_VSYNC=1`
makes `main.cpp` set the default `QSurfaceFormat` swap interval to 0, which Qt
maps to `QRhiSwapChain::NoVSync`, so the scene graph presents without waiting.
This measures how fast the application can produce frames, which is what both
of K1's clauses are really about, and it does not depend on the compositor's
cadence.

### Results — frame production (`RELDEX_S15_NO_VSYNC=1`), 1,000,000 rows

100% scale (`devicePixelRatio` 1):

| Phase | frames | p50 | p90 | p95 | **p99** | max | > 8 ms | > 16.7 ms | scene-graph p50 / p99 | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` | 3,000 | 5.31 | 6.01 | 6.06 | 6.31 | 6.65 | 0 | 0 | 0.035 / 0.072 | 0 |
| `flick` | 3,000 | 5.33 | 6.05 | 6.15 | 6.49 | 8.42 | 2 | 0 | 0.236 / 0.504 | 10,450 |
| `sweep` | 3,000 | 5.34 | 6.02 | 6.11 | 6.48 | 7.88 | 0 | 0 | 0.270 / 0.530 | 15,005 |
| `full` | 3,000 | **7.75** | 8.65 | 9.03 | **10.31** | 14.56 | 1,043 | 0 | 0.996 / 1.773 | **2,000,615** |
| `jump` | 3,000 | **7.82** | 8.87 | 9.45 | **10.84** | 11.91 | 1,173 | 0 | 1.009 / 1.861 | 998,638,991 |

150% scale (`QT_SCALE_FACTOR=1.5`, `devicePixelRatio` 1.5 confirmed):

| Phase | frames | p50 | p90 | p95 | **p99** | max | > 8 ms | > 16.7 ms | scene-graph p50 / p99 | rows travelled |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `idle` | 3,000 | 5.38 | 6.01 | 6.06 | 6.42 | 6.72 | 0 | 0 | 0.034 / 0.072 | 0 |
| `flick` | 3,000 | 5.36 | 6.04 | 6.16 | 6.49 | 9.04 | 1 | 0 | 0.241 / 0.514 | 10,450 |
| `sweep` | 3,000 | 5.38 | 6.04 | 6.15 | 6.52 | 8.75 | 1 | 0 | 0.271 / 0.520 | 15,005 |
| `full` | 3,000 | **7.69** | 8.61 | 8.97 | **10.19** | 12.40 | 975 | 0 | 0.987 / 1.693 | 2,000,616 |
| `jump` | 3,000 | **7.74** | 8.71 | 9.15 | **10.20** | 11.62 | 1,059 | 0 | 0.987 / 1.772 | 998,639,445 |

`rows travelled` is derived from the pixels the viewport actually moved divided
by the row height the view reports, so "2,000,615" is the whole result scrolled
down and back, not a claim about a shorter run.

### Verdict and how to read it

- **p99 clause: PASS, clearly.** The worst p99 anywhere is **10.84 ms** against
  a 16.7 ms threshold, and **not one frame in 30,000** exceeded 16.7 ms.
- **p50 clause: PASS, but read the floor.** The worst p50 is **7.82 ms**
  against 8 ms, which on its own is uncomfortably close. But the `idle`
  control — a window doing *nothing* — costs **5.31 ms per frame** on this
  setup. That floor is presentation overhead, not application work: the scene
  graph's own CPU work in the same phase is 0.035 ms. Subtracting it, the
  application's own contribution to the worst frame is about **2.5 ms**, which
  agrees with the independently measured scene-graph work of 1.0 ms p50 /
  1.9 ms p99 plus GUI-thread polish. The honest statement is: *the measured
  p50 passes, and the margin is much larger than the raw number suggests
  because most of that p50 is a fixed cost of this machine's present path.*
- **150% is not more expensive.** Every percentile at 1.5× device pixels is
  within noise of 100%, and `full` is marginally faster. Rendering cost here is
  dominated by scene-graph bookkeeping and cell formatting, not by fill rate.
- **The cache-hostile patterns cost about 2.4 ms more per frame than a gentle
  one** (`full`/`jump` p50 7.8 vs `sweep` 5.3), and that difference is where the
  model, the bulk formatter and the boundary live. It is real, it is bounded,
  and it never approaches a dropped frame at 60 Hz.

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

| Variant | n | execute → rows in model | execute → **first frame painted** |
| --- | --- | --- | --- |
| warm (2nd..31st execute) | 30 | p50 **1.30 ms**, p95 1.56, max 1.57 | p50 **11.16 ms**, p95 12.30, max 12.35 |
| cold process, window already drawn | 10 | p50 **2.01 ms**, p95 2.29, max 2.29 | p50 **431.82 ms**, p95 459.68, max 459.68 |
| cold process, execute at startup | 30 | p50 **190.65 ms**, p95 198.22, max 202.47 | p50 **621.97 ms**, p95 810.77, max 811.47 |

### Diagnosis

Against the 150 ms threshold the warm number passes by 13×, and the cold
number fails. The cost is located, and it is **not the boundary**:

1. **The boundary's own contribution is 1.3–2.3 ms.** Execute submitted →
   1,000 rows described, marshalled, crossed, and inserted into the model, in
   under 2.3 ms in every variant where the GUI thread is free.
2. **The 190 ms in the cold rows column is contention with application
   startup, not work.** It disappears entirely (2.01 ms) when the execute is
   held back 1.5 s. Submitting a query from `Component.onCompleted` means the
   reply cannot be drained until the QML engine and window finish initializing.
3. **The ~432 ms before the first painted frame is a once-per-process Qt cost
   that does not scale with anything.** Measured identically at **1,000 rows
   (429 ms)**, **100,000 rows (443 ms)** and **1,000,000 rows (432 ms)**, and
   identically whether a 1M-row stream is still in flight or the result was a
   single batch that completed immediately. It is also gone on the *second*
   query in the same process (11.16 ms). So it is the first frame that
   populates the `TableView`'s delegates — QML component instantiation and the
   graphics pipeline work that first frame triggers — paid once per process.

`AGENTS.md` forbids guessing, so what is **not** claimed: this report did not
profile inside Qt and does not apportion those 432 ms between delegate
instantiation, `TableView`'s first rebuild and D3D11 pipeline creation. What is
established is that it is fixed, once per process, independent of row count and
of the boundary, and that the same code path costs 11 ms every time after.

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
  before the first execute; sampled again 5 s after the result completed, with
  memory flat. Three runs each.

Both working set (`WorkingSetSize`) and private bytes (`PrivateUsage`) are
recorded. The ADR's metric is RSS growth, so the verdict is on working set,
with private bytes as context.

### Results

| Environment | run | RSS growth | B/row | private growth | B/row |
| --- | --- | --- | --- | --- | --- |
| headless | 1 | +115.97 MB | 116.0 | +117.94 MB | 117.9 |
| headless | 2 | +118.92 MB | 118.9 | +121.66 MB | 121.7 |
| headless | 3 | +114.11 MB | 114.1 | +114.90 MB | 114.9 |
| in-app | 1 | **+172.7 MB** | 172.7 | +142.1 MB | 142.1 |
| in-app | 2 | **+172.7 MB** | 172.7 | +142.0 MB | 142.0 |
| in-app | 3 | **+172.5 MB** | 172.5 | +142.0 MB | 142.0 |

**Verdict: PASS.** The worst reading is 172.7 MB against a 200 MB threshold,
reproducible to within 0.2 MB across three runs.

Two honest notes:

- **The baseline choice moves this number by 28 MB, and picking the wrong one
  would have produced a false failure.** Sampled from *process start* instead
  of from an idle drawn app, the same runs read **+200.7 MB** — over the
  threshold — because that charges the scene graph's, the RHI's and the
  graphics driver's one-time allocations to the result set. The driver records
  both; the verdict uses the one that answers "what did the rows cost".
- The headless figure (114–119 B/row) is essentially the ADR's own ~116 B/row
  estimate, and confirms M1.6's numbers after the ABI 3 change that stopped
  `reldex_batch_column` retaining unread `NUMBER`/`TIMESTAMP` mirrors
  (amendment A19). The in-app figure is ~55 B/row higher; that delta is the
  scene graph's and the view's, not the pipeline's.
- What is retained is still the whole fetched prefix (ADR-0003 D4's MVP rule).
  K3 passes *with* that; the result store (ADR-0004) is what changes it.

## K4 — per-batch boundary cost and per-cell `data()`

### Method

K4 has two halves in different units, and they are measured separately.

**Per batch:** `SessionController` times `ResultTableModel::applyBatch()` —
the call that takes the `ReldexBatch` the event delivered and calls
`reldex_batch_column` once per column, after which every column is viewable.
That is precisely "event delivered → batch described and its columns viewable".
Both clock reads are behind the same off-by-default flag as the recorder.

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
| in-app (scene graph running) | 2.6–2.9 µs | 16.2–17.5 µs | 24.2–170.6 µs | 3.4–3.6 µs |

Against the **200 µs** threshold: the worst single batch anywhere, in-app,
under a live scene graph, was **170.6 µs**, and the worst p99 was **17.5 µs**.
**PASS**, by roughly 11× at p99.

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

**Verdict: PASS**, with the ASan/UBSan/LSan evidence coming from Linux CI and
the Windows evidence coming from here. What is still true is that nobody has run
a sanitizer over this boundary on Windows, and MSVC's ASan would exercise a
different allocator and a different threading runtime; that is in Limitations.

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

### Results

| Phase | frames | p50 | p99 | max | frames > 16.7 ms | frames > 33 ms | model rows |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `sweep` (baseline) | 3,000 | 5.39 | 6.51 | 7.95 | 0 | **0** | 1,000,000 |
| `blockother` (second hub blocked 16.2 s) | 3,000 | 5.39 | 6.45 | 8.87 | 0 | **0** | 1,000,000 |
| `sweep` (baseline again) | 3,000 | 5.35 | 6.45 | 8.69 | 0 | **0** | 1,000,000 |
| `blocksame` (this session blocked) | 3,000 | 5.37 | 6.47 | 13.37 | 0 | **0** | 0 |

**Verdict: PASS.** Not one frame in 12,000 exceeded 33 ms, and the blocked
phase is statistically indistinguishable from the baseline that brackets it
(p50 5.39 vs 5.39/5.35; p99 6.45 vs 6.51/6.45). The one outlier worth naming is
`blocksame`'s 13.37 ms max, which is the frame on which the model reset and the
grid emptied — an artefact of the same-session block replacing the result, well
under budget, and attributable to the model reset rather than to the boundary.
Both baselines' own maxima (7.95 and 8.69 ms) bound what unrelated OS noise
looked like during this run, and `blockother`'s 8.87 ms sits inside that band.

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

**Verdict: PASS.** The worst `qt-build` job observed is **4.0 min**, six times
inside the 25-minute budget, and the cold run — the one without caches — was
the *fastest* Windows job of the set at 3.0 min, so caching is not what keeps it
under budget.

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

### Headless, 1,000,000 rows, all 16 combinations

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

### In-app, one run per size, 2 fetches in flight, `full` sweep

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
- **More than 2 fetches in flight buys nothing**: 2, 4 and 8 are within noise of
  each other, while 1 is measurably worse at small batch sizes (713 → 521 ms).

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
   as +200.7 MB — a false K3 failure. The driver now takes a second baseline on
   an idle, already-drawn app and reports both.

No change was made to `crates/**` or to `reldex.h`. The adapter changes are
listed in "What this task added".

## Limitations — what was NOT measured

- **Vsync-on frame cadence.** The desktop compositor was throttled to ~4 Hz for
  the whole session (display asleep; `DwmFlush` median 251.55 ms), and no
  available action would wake it without changing a system setting or
  synthesizing input on the owner's desktop. K1 is therefore answered from frame
  *production* with vsync off. A run on an awake desktop should be done before
  anyone quotes a 60 Hz swap distribution — and the ~5.3 ms idle presentation
  floor measured here is itself a property of the throttled state and would
  likely be smaller on a live desktop, which would only improve K1.
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
- **Miri** (ADR-0003 A9) — still not run; unchanged by this task.

## Recommendation for M1.9

**Accept ADR-0003, with two conditions recorded rather than resolved.** This is
advice; the lead and the owner decide.

The reasoning:

- **Every criterion whose subject is the boundary passes, most of them by a
  wide margin.** The per-batch crossing is 0.9 µs against a 200 µs budget; a
  warm cell is 100–120 ns against 200 ns; a million rows cost 114–119 B/row
  headless against a 200 B/row budget; teardown under a 10,000-iteration flood
  leaks nothing and hangs never, now with a clean ASan/UBSan/LSan run over the
  whole adapter suite on Linux CI behind it; a blocked session does not cost the
  UI a single frame over 33 ms in 12,000; the build is inside a sixth of its CI
  budget on all three runners.
- **The two non-green results are both located outside the boundary**, and
  located specifically, not hand-waved: K2's cold number is one-time Qt
  scene-graph work (the boundary's share of it is 2 ms, and the second query in
  the same process paints in 11 ms), and K7's single macOS failure was a build
  script race already fixed in M1.4.
- **K1 passes on both clauses**, and the p50 margin is much larger than the raw
  7.82 ms suggests once the 5.31 ms idle presentation floor is subtracted — but
  it was measured with vsync off, which is a real gap in the evidence.

The two conditions:

1. **Re-run K1 with vsync on, on an awake desktop, before the ADR's evidence
   section is considered complete.** Everything needed is in place and
   env-gated; it is one command. What it would add is the one thing this report
   cannot supply: the actual 60 Hz swap distribution. Nothing measured here
   suggests it will fail — the application produces frames in 7.8 ms p50 at its
   worst, well inside a 16.7 ms budget — but "nothing suggests it will fail" is
   not a measurement.
2. **Record the ~432 ms first-painted-frame cost as a known Phase 1 item with
   an owner.** It is not a boundary problem and it is not a reason to re-open
   ADR-0003, but it is the first thing a user will see on every launch, and
   `SPEC.md` §19's "warm desktop startup < 1 second desirable" is in the same
   neighbourhood. It wants a profile, not a guess.

Re-opening ADR-0003 is not warranted on this evidence. The hand-written C ABI
did what D4 claimed for it: text crosses zero-copy, the bulk formatter keeps a
warm cell at ~100 ns, one call per column per batch keeps the per-batch cost
three orders of magnitude inside its budget, and no scroll pattern — including
one that lands in a cold window every single frame — came within 6 ms of
dropping a frame at 60 Hz.

## Reproducing

Exact commands are in `ui/README.md`, "Reproducing the M1.8 numbers". The
summarised data behind every table is in
`docs/exec-plans/active/phase-1-s15-data/` (29 KB): `k1-k6-frames.csv`,
`k2-latency.csv`, `k3-memory.csv`, `k4-boundary.csv`, `sweep-headless.csv`,
`sweep-in-app.csv`, plus `summarize.py`, which reads a run's JSON and prints the
tables above. Per-frame raw samples are deliberately **not** committed — 30,000
frames per run is noise at this scale; `RELDEX_S15_CSV=<path>` re-emits them for
anyone who wants to check a percentile.

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
  `RELDEX_S15_RUNS` is set.
- `ui/adapter/Metrics.{h,cpp}` — `applyBatch` samples, scene-graph work samples,
  percentile/threshold-count aggregates, per-run K2 marks, a
  `firstFrameAfterInsert` signal, and a render-loop-thread check. All behind the
  existing `RELDEX_UI_METRICS` flag.
- `ui/adapter/Bridge.{h,cpp}` — owns the driver, and defers `autoStart()` to it
  when a measurement run wants a clean memory baseline.
- `ui/adapter/SessionController.cpp` — times `applyBatch` (K4) behind the
  metrics flag; `RELDEX_S15_AUTOFETCH`.
- `ui/app/main.cpp` — `RELDEX_S15_NO_VSYNC` sets the default swap interval to 0.
- `ui/app/Main.qml` — one line attaching the driver.
- `ui/tests/tst_resultmodel.cpp` — `spikeS15BoundaryCost`, skipped unless
  `RELDEX_S15_K4=1`, **with no timing assertion of any kind**: it measures and
  reports, and cannot fail on a number. No timing bound was added anywhere in
  the automated suite because of this work.
- `ui/README.md`, and this report.

**Merge note for M1.9.** This branch is based on `daa8d67`; `main` has since
moved to `57701853` (PR #19, the ASan CI job). That PR touched
`ui/CMakeLists.txt`, `ui/build.sh`, `.github/workflows/ui.yml`,
`ui/cmake/Sanitizers.cmake`, `ui/tests/lsan.supp`, `phase-1-toolchain.md` and
`ui/README.md`. **This task edited none of those except `ui/README.md`**, and
the two README edits do not overlap: #19's is the AddressSanitizer section
around line 427, this task's is the instrumentation section around line 301.
No merge was performed here.
