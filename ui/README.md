# Reldex UI

CMake + Corrosion + Qt Quick project holding the thin C++/Qt adapter over the
`reldex-ffi` C ABI, plus the smallest QML surface spike S15 needs.

Scope, per `AGENTS.md`: **M1.5** built the project skeleton (one `CoreInfo`
singleton exposing `reldex_abi_version()`, one window, one test). **M1.6**
added the adapter proper — `Bridge`, `SessionController`, `ResultTableModel`
and `Metrics` — and a `TableView` over the model. There is still no editor, no
toolbar, no theming and no settings; those are M3 onward, and building them
here would pre-empt the decision S15 exists to make.

## Prerequisites

- CMake >= 3.24, Ninja
- Qt 6.8 (LGPL open-source build; see "Licence" below)
- Rust stable (workspace-pinned toolchain) with the `x86_64-pc-windows-msvc`
  target (or your platform's default target on Linux/macOS)
- Network access the first time you configure (Corrosion is fetched via
  `FetchContent`; afterwards it is cached in the build tree)

On Windows, everything above except Rust is installed and pinned per
`docs/exec-plans/active/phase-1-toolchain.md`; `tools/dev-env/env.sh` puts it
all on `PATH` for the current shell only.

## Build

```bash
bash ui/build.sh --clean --test
```

- `--release` — configure `CMAKE_BUILD_TYPE=Release` instead of the default
  `RelWithDebInfo`. RelWithDebInfo is the default because this is a mixed
  Rust+C++ build and an unoptimized (`dev`-profile) `reldex-ffi` is
  noticeably slower once real batches are involved (M1.6+); RelWithDebInfo
  keeps optimizations and symbols for both languages, which is what the
  day-to-day inner loop wants.
- `--test` — run `ctest --output-on-failure` after building.
- `--clean` — remove the build directory first.

The build tree lands at `build/ui-<config>` at the **repository root**
(outside `ui/`), matching the project convention that generated trees stay
out of source directories; the root `.gitignore`'s existing generic
`build/` / `build-*/` patterns already cover it, so no `.gitignore` change
was needed for this task.

On Windows, `ui/build.sh` sources `tools/dev-env/env.sh` itself (detected via
`uname -s` matching `MINGW*`/`MSYS*`/`CYGWIN*`) — you do not need to source
it yourself first. On Linux/macOS the script assumes `cmake`/`ninja` are
already on `PATH` and Qt 6.8 is discoverable via `CMAKE_PREFIX_PATH` or
`Qt6_DIR`. This path is now exercised on every PR by `.github/workflows/ui.yml`
(`qt-build`, M1.4), which installs Qt via `jurplel/install-qt-action` and runs
this exact script on `ubuntu-latest` and `macos-latest` with
`QT_QPA_PLATFORM=offscreen` — so it is no longer untested, though nobody has
run it on a Linux/macOS **developer workstation** (as opposed to a fresh CI
runner) yet.

## Layout

```text
ui/CMakeLists.txt             Top-level: Qt, Corrosion/reldex-ffi, shared helpers
ui/cmake/                     CompilerWarnings.cmake, CopyIfDifferentRetry.cmake
ui/adapter/                   reldex_adapter: thin static lib + QML module (Reldex.Adapter)
  ReldexHandles.h              RAII wrappers for batch/error/arena + struct_size helpers
  CoreInfo.{h,cpp}             the ABI-version singleton (M1.5)
  Bridge.{h,cpp}               owns the ReldexHub, the waker, the budgeted drain, routing
  SessionController.{h,cpp}    open / execute / fetch / close; state and errors as properties
  ResultTableModel.{h,cpp}     QAbstractTableModel over borrowed batch views
  Metrics.{h,cpp}              S15 instrumentation, off by default
ui/app/                       Reldex executable + Main.qml (Reldex.App QML module)
ui/tests/                     QTest binaries, run via CTest
  tst_coreinfo.cpp             ABI version + Main.qml loads offscreen with no QML warning
  tst_bridge.cpp               drain budget/re-post, re-entrancy, deleteLater, error model,
                               blocked statement, routing
  tst_resultmodel.cpp          QAbstractItemModelTester, cell content, headers before the first
                               row, empty result, fetchMore, window cache, row ceiling,
                               re-execute mid-stream, live-count leak checks
  tst_teardown.cpp             spike criterion K5: 10,000 teardowns under a flood, asserted
                               with reldex_live_counts
  AdapterTestSupport.h         spin helpers, live-count baselines, the mock's values restated
ui/tests/ffi_smoke/           Qt-free C/C++ smoke harness for reldex-ffi (M1.4);
                              see "The ffi_smoke harness" below
ui/build.sh                   One-command build (bash-first; see AGENTS.md)
```

## The adapter (M1.6)

Three classes, one job each, in the shape ADR-0003 D1 fixes.

**`Bridge`** owns the `ReldexHub*`. It registers the waker; the callback runs
on a Reldex pump thread and does exactly one thing — a coalesced
`QMetaObject::invokeMethod(bridge, &Bridge::drain, Qt::QueuedConnection)`. It
calls no `reldex_*` function (the library answers `RELDEX_STATUS_REENTRANT`)
and lets no C++ exception escape into Rust. `drain()` takes events until the
queue is empty or the budget is spent (256 events / 4 ms, both writable), and
**re-posts itself** when it stops early, because the waker is edge-triggered on
empty → non-empty and no further wake is guaranteed. Events are routed to the
`SessionController` that owns their session; an event with no live owner has
its batch and error released and is counted in `orphanEvents()`.

**`SessionController`** sequences open → execute → fetch → close and owns the
session and result ids. It is where back-pressure lives, because the ABI has
none: the hub's event queue is unbounded and every `FETCHED` event hands over a
batch that becomes our memory, so `maxFetchesInFlight` (default 2) is the
bound. Errors are read out of `ReldexError` into plain properties and the error
object is freed exactly once by its RAII handle. "Exactly one reply per
accepted request" is a library guarantee, so it is asserted in debug rather
than defended against.

**`ResultTableModel`** owns the fetched batches and maps a row to
(batch, local row) with one binary search plus a last-hit cache — no object per
row anywhere. Column views are taken **once per column per batch**
(`reldex_batch_column`) when the batch arrives, never per cell, and
`rowCount` grows through `beginInsertRows`/`endInsertRows` per appended batch.
Roles are deliberately two: `display` and `isNull`, because NULL, empty and
taken are three different states and a grid must visualize the difference
rather than trust the text.

**Headers come from the result, not from a batch.** ABI 3 added
`reldex_session_result_column_count` / `reldex_session_result_column`, which
answer from the moment the `EXECUTED` event is drained. `SessionController`
reads the whole description there and hands it to `beginResult()`, so the model
gets its count *and* its names in one step, inside one reset, before a single
row exists. What that deleted: the two-phase header, the mid-stream reset for
the case where the event's `column_count` and the first batch disagreed, the
late `headerDataChanged`, and `m_columnCount` as a second source of truth
(`columnCount()` is now `m_columns.size()`, so the two cannot disagree). It
also made a result with **columns and no rows** representable at all — such a
result never produces a batch to read a header from.

Column names still reach QML through a notifying `columnNames` property rather
than through `headerData()`, and that part is not a workaround: a QML binding
on `model.headerData(...)` never re-evaluates, because `headerDataChanged` is a
model signal and not a property-change signal, so a header row bound that way
would keep whatever it read on the previous result. The headless QML test in
`tst_coreinfo` found that; reasoning about it had not.

### What `data()` is allowed to touch

`data()` makes **no** FFI call for a text cell: it reads `null_bits`,
`offsets` and `data` out of the column view it already holds and builds the
`QString` from those bytes. For every other kind — `NUMBER`, `TIMESTAMP`,
`BYTES`, `LOB`, `UNSUPPORTED`, and anything a future header adds — it reads
text the **bulk** formatter produced, through exactly one lazy path:
`ResultTableModel::hydrate()`, which calls `reldex_text_arena_create`,
`reldex_batch_format_column` over one **1,024-row window** of one column, and
`reldex_text_arena_view` once, and then caches the resulting `ReldexArenaView`
so every later cell in that (batch, column, window) is pointer arithmetic
again.

The window, rather than the whole batch, is what makes that cost independent of
`fetchRows`. D4 asks for "one call per visible window"; formatting a whole
batch *is* that when a batch is 1,000 rows and emphatically is not when it is
50,000 — measured during this task's independent review, same machine and
build, at 2.74 ms (`NUMBER`) and 7.55 ms (`DATE`) for the first cell read out
of a 50,000-row batch, which is a dropped frame the boundary would have been
blamed for. A window is also never split across batches, so no cell read ever
needs two renders.

Grep proof — every `reldex_*` call in `ResultTableModel.cpp`:

| function | what for | called from |
| --- | --- | --- |
| `reldex_batch_column` | describe a column once per batch | `applyBatch()`, rows > 0 only |
| `reldex_text_arena_create` / `reldex_batch_format_column` / `reldex_text_arena_view` | render one (batch, column, window) | `hydrate()` |
| `reldex_batch_release` / `reldex_text_arena_release` | RAII deleters in `ReldexHandles.h` | destruction |

That is the whole list — `reldex_batch_column_count` and
`reldex_batch_column_info` are gone from this file, because the header no
longer comes from a batch. Column descriptions are read once per *result*, in
`SessionController::readResultColumns()`, with
`reldex_session_result_column_count` and `reldex_session_result_column`.

**`reldex_batch_column_fixed` is deliberately never called.** ABI 3 made
`reldex_batch_column` allocation-free: for `NUMBER` and `TIMESTAMP` it leaves
`fixed` NULL and reports only `fixed_stride`, and the element array is built
only if someone asks for it — 46 B/row for `NUMBER`, 16 B/row for a timestamp,
retained until the batch is released. This adapter renders every non-text kind
through the bulk formatter, so it would never read those arrays. Not calling it
is what closed the K3 gap measured in round 1 (see the numbers below).

### Caching and its memory bound

Formatted text is held per **(batch, column, 1,024-row window)** in its own
`ReldexTextArena`. The arenas sit in a least-recently-used list bounded by
*both* a count (`maxFormattedWindows`, default 64) and a byte total
(`maxFormattedBytes`, default 8 MiB); whichever bites first evicts from the
least-recently-used end. Reading a cell moves its window to the front, and the
window just rendered is never the victim, so the bound can be exceeded by at
most one window and a cell read is never followed by a re-render of the window
it was read from.

The bound matters because K3 allows 200 MB of RSS growth for 1,000,000 rows,
and retaining formatted text for all of them would spend most of that on
strings nobody is looking at. Measured on the S14 shape (`tst_resultmodel`
reports these): one 1,024-row window costs **11,189 bytes** for `ID`
(`NUMBER`) and **18,440 bytes** for `CREATED` (`DATE`) — the UTF-8 bytes plus
one 8-byte offset per row. So the default 64-window bound is about **1.2 MB**
for this shape, and the 8 MiB byte bound is what actually holds when a column
is wide (it caps the cache at ~450 windows of 18 KB, or fewer of anything
wider). Either way it is independent of how many rows have been fetched.

The row data itself *is* retained — ADR-0003 D4 says the MVP keeps the fetched
prefix, and ADR-0004 (result store) is where that changes.

`ResultTableModel` also has a hard row ceiling (`maxRows`, default `INT_MAX`),
because `QAbstractItemModel` counts rows in `int` and a stream long enough to
overflow one has to stop rather than wrap. Reaching it warns once, sets
`rowLimitReached`, truncates the batch that crossed it rather than dropping it,
and makes `canFetchMore()` false so nothing further is fetched. What was
already fetched stays readable.

A `QString` cache per visible cell was considered and rejected: it would add a
second copy of the same text (a `QString` is 24 bytes plus a heap block) for a
saving only on repeated reads of the same cell, and it would make the memory
bound depend on how a view happens to call `data()`. What the current shape
costs instead is one `QString::fromUtf8` per `data()` call, which is precisely
the number S15's K4 measures — so it is left visible rather than optimized away
before it has been measured.

### What `Main.qml` deliberately does not do

The `TableView` uses **constant** `columnWidthProvider` and `rowHeightProvider`
(220 × 22). Auto-sizing is a different cost class: with no explicit provider a
`TableView` asks the delegate for an implicit size, and a width derived from
content has to look at rows the viewport is not showing — which is a
`data()` call per candidate row, on a model whose whole design is that
`data()` is only ever asked about visible cells. Column sizing is a real
feature with its own decisions to make (sample the first N rows? remember the
user's drag?), and it belongs to the editor milestone, not to a spike about
the boundary. Fixed providers keep the numbers above measuring the boundary
rather than a sizing policy nobody has chosen yet.

### Teardown (ADR-0003 D5 rule 2 and A10)

`~Bridge`, in this order:

1. `reldex_hub_set_waker(hub, NULL, NULL)` — does not return while a wake is in
   progress, so the trampoline can never see a half-destroyed `Bridge`;
2. delete the `SessionController` and its model, which releases every
   `ReldexBatch` they hold — `reldex_hub_destroy` requires that;
3. drain whatever is still queued and release the batches and errors it
   carries (the queue is unbounded and an undrained batch is our memory);
4. `reldex_hub_destroy`. A10 requires that no other thread is inside any
   `reldex_*` call, *including* `reldex_session_request_cancel`. This adapter
   makes every call on the `Bridge`'s own thread and never uses the
   cancel-from-any-thread allowance, so there is no thread to join and nothing
   to synchronize;
5. `~QObject` removes any drain this object had posted for itself, so a queued
   invocation can never reach a dead `Bridge`.

Two rules follow for anything a drain can reach, and both are stated in
`Bridge.h` where they are implemented:

- **`drain()` is never re-entered.** A nested event loop — a modal dialog, or a
  `QEventLoop` spun inside a handler — delivers the drains posted while an
  outer drain is still walking the queue. Re-entering would dispatch events
  inside an outer dispatch, mutating the model mid-signal and applying events
  in an order the library did not produce. The delivery bails out and re-posts
  instead. `tst_bridge` proves it: no `drained` signal is emitted while a
  nested loop runs inside a drain, and the stream still completes with every
  row and no orphan.
- **Destroy the `Bridge` with `deleteLater()`, never `delete`, from inside
  anything a drain calls.** `~Bridge` would tear the hub down underneath the
  loop still walking it; `deleteLater()` defers that to the next return to the
  event loop, which is after the drain has finished. Also covered by
  `tst_bridge`, with fetches still in flight at the moment of destruction.

## Instrumentation, and how M1.8 runs the S15 measurement

`ui/adapter/Metrics.{h,cpp}` holds every hook, compiled in and **off by
default**: each recording entry point is an inline `if (!isEnabled()) return;`
in front of an out-of-line implementation. The flag is a
`std::atomic<bool>` read relaxed, because `frameSwapped` is connected
`Qt::DirectConnection` and therefore reads it from the render thread while the
GUI thread may be writing it — a plain `bool` there is a data race whatever it
would do in practice. It records monotonic marks (execute submitted → first
event → first rows inserted → first frame swapped after that insert), per-drain
event counts and durations, frame intervals from `QQuickWindow::frameSwapped`,
and memory (resident set size *and* private bytes — on Windows the working set
is shared-page-inflated and K3 is about what this process actually owns, so
`privateBytes()` is the one to quote and both are recorded). It draws no
conclusions —
`writeCsv()` emits `section,a,b,c` rows (`mark`, `drain`, `frame`) and the
spike report is M1.8's to write.

`frameSwapped` is connected `Qt::DirectConnection` on purpose: it is emitted on
the render thread, and queueing it to the GUI thread would time the GUI
thread's backlog rather than the frame. Samples are appended under a mutex and
capped at 200,000 so the instrument never becomes part of the K3 answer.

### Environment variables

| Variable | Default | What it does |
| --- | --- | --- |
| `RELDEX_UI_METRICS` | off | any value but `0` enables recording |
| `RELDEX_S15_AUTORUN` | off | any value but `0` makes `Main.qml` run the query on load |
| `RELDEX_S15_ROWS` | `1000000` | rows the mock generates |
| `RELDEX_S15_SEED` | `0` | mixed into the generated values |
| `RELDEX_S15_FETCH_ROWS` | `1000` | `max_rows` per fetch |
| `RELDEX_S15_FETCHES_IN_FLIGHT` | `2` | the adapter's back-pressure bound |
| `RELDEX_S15_PER_FETCH_LATENCY_US` | `0` | simulated per-fetch round trip |
| `RELDEX_S15_FIRST_BATCH_LATENCY_US` | `0` | extra cost before the first batch |
| `RELDEX_UI_TEARDOWN_ITERATIONS` | `10000` | K5 iterations in `tst_teardown` |
| `RELDEX_UI_TEARDOWN_CONNECT_ITERATIONS` | `2000` | K5's connect-window variant |
| `RELDEX_UI_SANITY_1M` | off | enables the skipped 1M-row sanity stream in `tst_resultmodel` |

### Launching for a measurement run

```bash
source tools/dev-env/env.sh
RELDEX_UI_METRICS=1 RELDEX_S15_AUTORUN=1 RELDEX_S15_ROWS=1000000 \
  ./build/ui-RelWithDebInfo/Reldex.exe
```

`Reldex.exe` is a GUI-subsystem binary, so scroll it by hand and read the
numbers from `bridge.metrics.summary()` / `bridge.metrics.writeCsv(path)`; the
`Metrics` object is reachable from QML as `bridge.metrics`. Everything except
the frame-time half can also be driven headless:

```bash
RELDEX_UI_SANITY_1M=1 ./build/ui-RelWithDebInfo/tst_resultmodel.exe \
  sanityStreamOfAMillionRows -o run.txt,txt
```

**Two things `QAbstractItemModelTester` does that a test has to plan around.**
It re-runs its whole suite on every `rowsInserted`, and that suite **walks the
model's rows** — so attaching it to a long stream makes the *test* quadratic,
not the model (a 200,000-row stream went from under a second to minutes).
It also calls `fetchMore()` itself, from `nonDestructiveBasicTest()`, so with a
tester attached the model drives its own stream no matter what `autoFetch`
says; a test that needs "no rows have arrived yet" has to sample that from a
signal emitted before the first fetch, not from a wait. Both were found by
tests flaking, not by reading Qt's source first.

**K5 iteration count on CI.** `tst_teardown` keeps its full 10,000 + 2,000
iterations on CI: 6.3 s here, and even an order of magnitude slower on a
two-core runner leaves the `qt-build` job's 25-minute budget untouched.
`RELDEX_UI_TEARDOWN_ITERATIONS` / `RELDEX_UI_TEARDOWN_CONNECT_ITERATIONS` are
the lever if the first real Linux/macOS run says otherwise. The live-count
waits in that test use a 300-second hang guard for the same reason — it waits
for up to 10,000 pump threads to finish (A17), and that is a hang guard, not a
latency bound.

**Reading a test's own output on this machine:** a Qt test binary's stdout does
not reach a redirected file or a pipe from Git Bash or PowerShell here (the
same class of problem as `phase-1-toolchain.md` §12 gotcha 4; CTest's
`LastTest.log` records `<end of output>` for a test that demonstrably ran and
printed). Use QTest's own file logger — `-o <file>,txt` — which does work, and
is how the numbers below were read. The test targets are forced to the console
subsystem (`WIN32_EXECUTABLE FALSE`) because `qt_add_executable()` defaults it
to `TRUE`; that is necessary but, on this machine, not sufficient.

### Numbers seen while building this (information only — M1.8 owns the real ones)

Dev machine, Windows 11, MSVC 14.44, Qt 6.8.3, `RelWithDebInfo`, mock driver,
guiless (no scene graph), 1,000,000 rows of the S14 shape, 1,000 rows per
fetch, 2 fetches in flight:

- streamed into the model in **453–470 ms**;
- RSS grew by **113.0–118.1 MB** (113–118 B/row), private bytes by
  **113.7–119.5 MB** (114–120 B/row) — comfortably under K3's 200 MB and at or
  below the ADR's own ~116 B/row estimate, and this is *without* a scene graph;
- a cold 40-row × 3-column viewport (which renders two 1,024-row windows
  through the bulk formatter) took **~240–261 µs**; the same viewport warm
  averaged **99.4–100.9 ns/cell**, against K4's 200 ns budget. The
  formatted-text cache held **2 windows / 41,016 bytes** at the end of that —
  the bound in action on a 1,000,000-row result;
- K5: **10,000 teardowns under a flood in 6.34 s** (0.634 ms each), RSS +2.5 MB
  across the whole loop, and — new in ABI 3 — `reldex_live_counts` back to its
  baseline of zero hubs, sessions, batches, errors and arenas.

And with the real app (`Reldex.exe`, scene graph, a `TableView` on screen),
sampled from outside the process:

- idle, nothing executed: **76.2 MB** working set / **82.5 MB** private, flat;
- after the 1,000,000-row query (flat from ~27 s, stable out to 72 s):
  **247.5 MB** working set / **224.1 MB** private — a growth of **171.3 MB**
  working set, **141.5 MB** private.

**K3 went from "the tight one" to "clear with headroom", and the cause is worth
recording.** Round 1 of this task measured ~181 B/row headless and ~231 MB of
in-app growth, both near or past K3's 200 MB threshold. The independent review
diagnosed it — not `Vec` capacity slack, and not on this side of the boundary:
`applyBatch()` calls `reldex_batch_column` once per column, and under ABI 2 a
`NUMBER` or `TIMESTAMP` column answered by building and then **retaining** a
`#[repr(C)]` mirror (46 B/row and 16 B/row) that this adapter never reads,
because those kinds go through the bulk formatter. Measured directly at the
time: 111.7 B/row with no column viewed, 183.3 B/row with all three viewed.

ABI 3 fixed it in the producer (amendment A19): `reldex_batch_column` allocates
nothing, `fixed` is NULL for those kinds, and the mirror is built only by the
new `reldex_batch_column_fixed`, which this adapter does not call. The adapter
needed no change for it, which is the point — the numbers above are the same
code paths measured against the new boundary. Per the ADR: "K1/K3 failure with
a *diagnosed* cause in the model (not the boundary) is a design fix, not a
kill." It was, and it was fixed.

What remains is still the *retained fetched prefix*: ADR-0003 D4 has the MVP
keep every batch it has fetched, so a scrolled-through million-row result holds
a million rows of batch memory. That is ADR-0004 (result store) territory, and
K3 now passes without it. The formatted-text cache is not a factor either way —
it is bounded at about 1.2 MB for this shape, and held 41 KB in practice.

One in-app number worth flagging for M1.8: the 1,000,000-row stream takes
**~25 s with the scene graph running** against ~0.46 s headless. Nothing blocks
— the window stays responsive throughout — but that is a K1/K2 question
(how much UI-thread time a drain competes for) rather than a K3 one, and it is
M1.8's to measure properly with `RELDEX_UI_METRICS=1`.

No frame-time number is claimed: K1 needs a real swapchain and a scrolling
window driven by a human or a harness, which is M1.8's job on the dev machine,
not this task's. The hooks for it are in place and were exercised (the app runs
with `RELDEX_UI_METRICS=1` and stays responsive throughout the stream).

### AddressSanitizer: not available on this machine

`/fsanitize=address` was attempted in a separate build directory and **cannot**
be made to work here: this Visual Studio 2022 Community install ships only the
**32-bit** ASan runtime (`clang_rt.asan_dynamic-i386.lib`,
`clang_rt.asan_dynamic_runtime_thunk-i386.lib`); there is no x64 counterpart,
so the link fails with
`LNK1104: cannot open file 'clang_rt.asan_dynamic_runtime_thunk-x86_64.lib'`.
Building 32-bit instead is not an option: the installed Qt is
`msvc2022_64` only and `reldex-ffi` is built for `x86_64-pc-windows-msvc`.

Getting it would mean installing the "C++ AddressSanitizer" component into the
Visual Studio installation — a machine-level change, which this repository's
scripts and tasks do not make. Linux ASan/UBSan in CI (ADR-0003 D2/D10) is the
intended home for this: the `ffi-smoke` job in `.github/workflows/ui.yml`
already runs its ubuntu leg under ASan+UBSan with `detect_leaks=1` on the
Qt-free C/C++ harness, and `qt-asan` (below) does the same for this whole Qt
Quick tree, which is what actually exercises K5.

### Building this whole tree under ASan/UBSan (`--sanitize`, K5)

```bash
bash ui/build.sh --sanitize --test
```

`RELDEX_SANITIZE` (`ui/CMakeLists.txt`, implemented in
`ui/cmake/Sanitizers.cmake`) is a project-wide CMake option, GCC/Clang only:
it adds `-fsanitize=address,undefined -fno-omit-frame-pointer -g` to
`reldex_adapter`, `Reldex`, every QTest binary, and (reusing the same option
name, already declared) the `ffi_smoke` harness. It never recompiles Qt
itself (found prebuilt via `find_package`) or `reldex-ffi`'s Rust cdylib
(built by cargo via Corrosion) — but once ASan's runtime is linked into an
executable, it still intercepts `malloc`/`free` calls that uninstrumented
code in the same process makes, which is what lets it see across that
boundary at all. On MSVC the option is accepted, warns once per target, and
is otherwise ignored (mirroring `ui/tests/ffi_smoke`'s own long-standing
behaviour) — this is how it degrades on the Windows dev machine described
above.

`--sanitize` builds into a separate directory (`build/ui-<config>-asan`), and
with `--test` it also sets three runtime option strings for the `ctest` run
and passes `ctest -V` (verbose: every test's own output lands in the log, not
just failing ones — the only way to see `tst_teardown`'s printed iteration
count and its `reldex_live_counts` assertions as evidence rather than just a
pass/fail line):

- `ASAN_OPTIONS=detect_leaks=1:abort_on_error=1:strict_string_checks=1`
- `UBSAN_OPTIONS=print_stacktrace=1:halt_on_error=1`
- `LSAN_OPTIONS=suppressions=ui/tests/lsan.supp`

`ui/tests/lsan.supp` exists because Qt, fontconfig and the GL/offscreen stack
can report leaks that are not ours; per that file's own header, an entry is
added there **only** after a CI run's full stack confirms the leak originates
entirely outside `ui/adapter`, `ui/tests` and `reldex_ffi` — anything in our
own frames is a finding to fix, never something to suppress.

CI runs this as `qt-asan` in `.github/workflows/ui.yml` (ubuntu-latest only,
same reasoning as `ffi-smoke`'s ASan leg: GCC/Clang required, one OS is
enough to prove the boundary sanitizer-clean), keeping `tst_teardown`'s full
10,000 + 2,000 iteration count (`RELDEX_UI_TEARDOWN_ITERATIONS` /
`_CONNECT_ITERATIONS` are the lever if a slower runner ever needs it
reduced — see that variable's row above).

It matters less than it did. ABI 3 added `reldex_live_counts`, which reports
how many hubs, sessions, batches, errors and arenas the library holds — so K5's
leak claim is now an **assertion** rather than an argument from RSS. Every
teardown test takes a baseline before it starts and waits for the counts to
return to it: the 10,000-iteration flood, the connect-window variant, the
`deleteLater()`-mid-drain teardown, the error path, and the mid-stream result
reset. Two details the header is explicit about and the tests follow:

- it is always a **wait**, never an immediate compare, because A17 says
  `reldex_hub_destroy` does not join the session pump threads;
- the baseline is taken **before** anything is created and after the library
  has gone quiescent. Taking it while a previous test's pump thread was still
  finishing made a test fail for having *fewer* live objects than it started
  with — found that way, not by reasoning.

What the counters cannot see is a leak on our own side of the boundary; the
RAII handles in `ReldexHandles.h` and the flat RSS across 12,000 teardowns
remain the evidence for that half.

### Why `ui/tests` re-attaches `../app/Main.qml` instead of sharing a library

The natural design is a small `reldex_app_qml` static library holding
`Main.qml`, linked by both `Reldex` and `tst_coreinfo`. It does not work:
a *statically linked entry-point* QML module (one loaded imperatively via
`loadFromModule`, never `import`-ed by another `.qml` file) only
self-registers when it is compiled directly into the binary that loads
it. Nothing in `main.cpp` references any symbol from a separate
`reldex_app_qml.lib`, so the linker is free to drop the whole archive —
confirmed by hand: the build succeeds warning-free, and the app fails at
*runtime* with `No module named "Reldex.App" found`.

The fix used here: `qt_add_qml_module()` is called directly on **both**
`Reldex` (`ui/app/CMakeLists.txt`) and `tst_coreinfo`
(`ui/tests/CMakeLists.txt`), each pointing at the same `ui/app/Main.qml`
file (not a copy). Each executable gets its own compiled copy of the
`Reldex.App` module in its own binary, which is exactly what makes
self-registration reliable. `ui/tests/CMakeLists.txt` gives its copy an
explicit `OUTPUT_DIRECTORY` (`qml-tests/Reldex/App` instead of the default
`qml/Reldex/App`) so the two do not collide, and `NO_CACHEGEN` because the
AOT qmlcache compiler cannot name its intermediate files sanely for a
source file outside the target's own directory tree.

### Why `QT_QML_OUTPUT_DIRECTORY` is set at all

`reldex_adapter`'s `Reldex.Adapter` module is consumed by `import
Reldex.Adapter` from `Main.qml`. A *library*-backed QML module's default
`OUTPUT_DIRECTORY` (with `QT_QML_OUTPUT_DIRECTORY` unset) is just
`CMAKE_CURRENT_BINARY_DIR`, with **no target-path suffix** — harmless on
its own (just a `qmllint` warning), except that a consumer's
`qmlimportscanner`-driven static-plugin linking uses that same convention
to find the module. Without the fix, `Reldex.exe` and `tst_coreinfo.exe`
both built and linked with **zero warnings**, and then both failed at
*runtime* with `module "Reldex.Adapter" plugin "reldex_adapterplugin" not
found` the moment they tried to load `Main.qml`. Setting
`QT_QML_OUTPUT_DIRECTORY` project-wide (`ui/CMakeLists.txt`) fixes both the
warning and the runtime failure, because it is also what
`_qt_internal_collect_qml_import_paths()` adds to every consumer's import
path automatically.

## The `ffi_smoke` harness (M1.4)

`ui/tests/ffi_smoke` is a plain **C11** program (also compiled as **C++17**,
from the same source, to prove `crates/ffi/include/reldex.h` is C++-clean
too) that links `reldex-ffi`'s cdylib directly and drives the mock driver
end to end: ABI version check, hub, waker, session open, execute, fetch
every batch (row counts, a text column via its offsets/data view, a
`NUMBER` mirror, the null bitmap, column names, a formatted column through
a `ReldexTextArena`), a failing execute, `struct_size` forward/backward
compatibility, then a clean teardown. **No Qt** anywhere in this directory
(ADR-0003 D10 item 2) — it is the boundary test that has to pass before the
Qt half of the stack is even worth building.

It builds two ways:

```bash
# Standalone (has its own project(), fetches Corrosion itself):
bash ui/tests/ffi_smoke/run.sh --clean

#   --sanitize   -fsanitize=address,undefined for the C/C++ targets
#                (GCC/Clang only; reldex-ffi's Rust code is never
#                instrumented, but ASan still intercepts its malloc/free
#                calls from this process). Sets ASAN_OPTIONS=detect_leaks=1
#                for the ctest run.
bash ui/tests/ffi_smoke/run.sh --sanitize --clean

# As part of the full UI build (ui/tests/CMakeLists.txt add_subdirectory's
# it; it reuses reldex_ffi-shared and RELDEX_FFI_INCLUDE_DIR from
# ui/CMakeLists.txt rather than importing Corrosion a second time):
bash ui/build.sh --test
```

Both `reldex_ffi_smoke_c` and `reldex_ffi_smoke_cpp` are registered with
CTest and print one `[PASS]`/`[FAIL]` line per check; exit code 0 only if
every check passed. `RELDEX_SANITIZE=ON` is a CMake option on that
directory alone — it never instruments `reldex-ffi` itself, only the C/C++
harness targets, on GCC/Clang.

`.github/workflows/ui.yml`'s `ffi-smoke` job runs this on all three OSes
(no Qt install needed), with ASan+UBSan enabled on the ubuntu leg.

## Dependencies

**Corrosion** (`corrosion-rs/corrosion`, MIT licence) drives `cargo` from
CMake (ADR-0003 D8): Qt's own tooling (`qt_add_qml_module`, moc,
`windeployqt`, the mobile packaging targets) is CMake-native, so inverting
that — driving CMake from cargo — costs far more than one
`corrosion_import_crate()` call. It also handles target triples, build
profiles, and the platform link libraries Rust's std needs on Windows
(`ntdll`, `userenv`, `bcrypt`), which the configure log for this project
confirms it resolved automatically.

Pinned at tag **`v0.6.1`**, commit `1499b14e4906a2890f5cee1547c8848db261753d`
(the latest tagged release at the time this was written — checked via the
GitHub API, not guessed). `ui/CMakeLists.txt` pins by commit, not by the
mutable tag name, so a re-tag upstream cannot silently change what gets
built; the tag name is kept alongside it purely for a human to recognise
the version at a glance. Corrosion is fetched with `FetchContent` into the
build tree — it is a CMake-only dependency (no Cargo.toml entry, no Rust
crate), matching `AGENTS.md`'s "no production dependency without
documenting why."

`reldex-ffi` is imported by package name (`CRATES reldex-ffi`) from the
workspace `Cargo.toml` one directory up. Its `[lib] name = "reldex_ffi"`
(underscored) is what Corrosion uses for the generated CMake target names:
`reldex_ffi-static` and `reldex_ffi-shared`.

## Library kind and DLL deployment

ADR-0003 D8/D9 leaves desktop linkage as "cdylib on desktop, staticlib
reserved for iOS." This project links **`reldex_ffi-shared`** (the cdylib)
into `reldex_adapter`, which is `PUBLIC`, so it propagates to every
consumer (`Reldex`, `tst_coreinfo`) without each one repeating it.
`reldex_ffi-static` is imported by Corrosion but unused so far — nothing in
this skeleton needs it; it stays available for when an iOS target links it
directly, per D9.

Windows has no rpath, so `reldex_ffi.dll` (and its `.pdb`) must sit next to
every executable that (transitively) links `reldex_ffi-shared`. A small
CMake function in `ui/CMakeLists.txt`, `reldex_deploy_ffi_dll(<target>)`,
adds a `POST_BUILD` step that copies it via
`ui/cmake/CopyIfDifferentRetry.cmake` (a plain `copy_if_different` wrapped
with a retry — M1.4's CI hit a transient "source not found" from a bare
`copy_if_different` under enough ninja parallelism on macOS once
`ui/tests/ffi_smoke` added two more targets doing the same copy; see that
script's header comment); it is called once each for `Reldex` and
`tst_coreinfo`. Qt's own DLLs are resolved via `PATH` (`tools/dev-env/env.sh`
puts Qt's `bin/` there); no `windeployqt` step exists yet because this is a
dev-build skeleton, not packaging (that is a later M6 task).

Rust's own build artefacts (the actual `target/` cargo uses) live inside
the CMake build tree, under `build/ui-<config>/cargo/`, which is Corrosion's
default and is deliberately left as-is — the disk cost (a few hundred MB
per configuration) is an acceptable trade for not needing a second,
separately-managed Cargo target directory. It is git-ignored along with
the rest of `build/`.

## Licence note

Qt 6.8 is used under **LGPLv3, dynamically linked only**
(`find_package(Qt6 ...)` + `qt_add_executable`/`qt_add_library` link Qt
the normal shared-library way; nothing here builds or links a static Qt).
Only `Core`, `Gui`, `Qml`, `Quick`, `Test` are used — no GPL-only Qt module,
no Qt Creator dependency. See
`docs/exec-plans/active/phase-1-toolchain.md` §4–5 for the full inventory
of what was installed and the explicit confirmation that no GPL-only
module (`Qt6Charts`, `Qt6WebEngineCore`, etc.) is present.

## Known limitations

- **Linux/macOS are CI-tested, not developer-workstation-tested.** M1.4
  added `.github/workflows/ui.yml`, which builds and runs this whole tree
  (`ffi-smoke` standalone, `qt-build` full Qt Quick build + `ctest`,
  offscreen) on `windows-latest`/`ubuntu-latest`/`macos-latest` on every PR.
  That proves the non-Windows CMake path (Qt via `CMAKE_PREFIX_PATH`,
  Corrosion, `ui/build.sh`'s non-Windows branch) works on a fresh CI runner;
  nobody has yet run it by hand on a real Linux/macOS development machine
  (only Windows 11 + MSVC 2022 + Qt 6.8.3 was available for that).
- **No `windeployqt` / packaging step.** Qt DLLs are found via `PATH` in
  this dev environment; a real installer/package needs `windeployqt` (or
  the CMake `qt_generate_deploy_app_script()` equivalent), which is out of
  scope for a build skeleton.
- **The mock driver is the only driver.** `SessionController::open()` builds a
  `ReldexOpenOptions` with `RELDEX_DRIVER_KIND_MOCK`, because that is the only
  kind this build of `reldex-ffi` accepts (ADR-0003 A8). Connection profiles
  and a real driver are M2/M3.
- **LOBs do not cross the boundary yet** (ADR-0003 A7). A LOB column reports
  its kind and the bulk formatter renders it as the "taken" text; there is no
  handle to read from. That is M2.11.
- **No cancel affordance.** `reldex_session_request_cancel` is deliberately not
  called anywhere in this adapter: using it would create the cross-thread
  sequencing obligation A10 describes, and nothing in M1.6 needs it. The
  `cancel_kind` an `OPENED` event reports is stored (`cancelKind()`) and
  otherwise unused.
- **One session per `Bridge`.** The routing table is a
  `QHash<sessionId, QPointer<SessionController>>` and handles any number, but
  the `Bridge` creates exactly one controller, because the spike needs one.
  Several worksheets are M3.
- **No AddressSanitizer on this machine** — see the section above for exactly
  why, and what stands in for it.
- **The offscreen platform plugin warns about missing fonts**
  (`QFontDatabase: Cannot find font directory .../lib/fonts. ... Qt no
  longer ships fonts.`) every run, since this Qt install has none deployed.
  It is a `qWarning()` from Qt's font subsystem, not a QML warning, so it
  does not fail `tst_coreinfo` (which only asserts on `QQmlEngine::warnings`
  emissions) — but it will show up in CTest/CI logs and is worth knowing
  about rather than mistaking for a real regression.
