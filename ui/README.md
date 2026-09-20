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
`Qt6_DIR`; **that path has not been exercised** by whoever last verified
this file — only Windows was available. If something is wrong with it,
that is the first thing to check.

## Layout

```text
ui/CMakeLists.txt             Top-level: Qt, Corrosion/reldex-ffi, shared helpers
ui/cmake/                     CompilerWarnings.cmake (the only CMake helper module)
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
  tst_resultmodel.cpp          QAbstractItemModelTester, cell content, fetchMore, window cache,
                               row ceiling, re-execute mid-stream
  tst_teardown.cpp             spike criterion K5: 10,000 teardowns under a flood
  AdapterTestSupport.h         spin helpers + the mock's generated values, restated
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

Column names reach QML through a notifying `columnNames` property rather than
through `headerData()`. A QML binding on `model.headerData(...)` evaluates
once — before the first batch has brought the names, because the *count* comes
from the `EXECUTED` event and the *names* come from a batch (ADR-0003 A6) — and
never re-evaluates, since `headerDataChanged` is a model signal, not a
property-change signal. The header showed "1", "2", "3". The headless QML test
in `tst_coreinfo` found that; reasoning about it had not.

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

| line | function | called from |
| --- | --- | --- |
| `reldex_batch_column` | describe a column once | `applyBatch()` |
| `reldex_batch_column_count` / `reldex_batch_column_info` | column names | `readColumnHeaders()` |
| `reldex_text_arena_create` / `reldex_batch_format_column` / `reldex_text_arena_view` | render one (batch, column, window) | `hydrate()` |
| `reldex_batch_release` / `reldex_text_arena_release` | RAII deleters in `ReldexHandles.h` | destruction |

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

- streamed into the model in **451–485 ms**;
- RSS grew by **~176–182 MB** (~176–182 B/row; private bytes ~178 B/row) —
  under K3's 200 MB, but well above the ADR's ~116 B/row estimate, and this is
  *without* a scene graph. M1.8 should expect K3 to be the tight one (the cause
  is diagnosed below);
- a cold 40-row × 3-column viewport (which renders two 1,024-row windows
  through the bulk formatter) took **~248 µs**; the same viewport warm averaged
  **~100 ns/cell**, against K4's 200 ns budget. The formatted-text cache held
  **2 windows / 41 KB** at the end of that — the bound in action on a
  1,000,000-row result;
- K5: **10,000 teardowns under a flood in 6.85 s** (0.685 ms each), RSS +2.6 MB
  across the whole loop.

And with the real app (`Reldex.exe`, scene graph, a `TableView` on screen),
sampled from outside the process:

- idle, nothing executed: **76 MB** working set, flat;
- after the 1,000,000-row query (CPU flat by ~2 s, working set flat by ~4 s):
  **307 MB**, stable out to 40 s — a growth of **~231 MB**.

**This is the number M1.8 should look at first.** K3's threshold is 200 MB of
RSS growth for 1M rows and both measurements are near or past it (~180 MB
headless, ~231 MB with the UI). The formatted-text cache is not the cause — it
is bounded at about 1.2 MB for this shape. What costs the memory is the
*retained fetched prefix*: ADR-0003 D4 has the MVP keep every batch it has
fetched, so a scrolled-through million-row result holds a million rows of batch
memory.

The gap between the measured ~181 B/row and the ADR's ~116 B/row estimate has
since been **diagnosed** (independent review of this task, same machine and
build): it is not `Vec` capacity slack, and it is not on this side of the
boundary. `applyBatch()` calls `reldex_batch_column` once per column, and for
`NUMBER` and `TIMESTAMP` columns `crates/ffi` answers by building and then
**retaining** a `#[repr(C)]` mirror of that column — 46 B/row for `ReldexNumber`
and 16 B/row for the timestamp — which the adapter never reads, because those
kinds go through the bulk formatter instead. Measured directly: **111.7 B/row**
with no column viewed, **183.3 B/row** with all three viewed. The two mirrors
account for the whole difference.

The fix belongs in `crates/ffi` (do not materialize a mirror a caller has not
asked for, or drop it once the view is released), and the lead is doing it in a
separate ABI round; no adapter-side workaround was added here, because working
around it would mean *not* taking column views, which is the thing the
zero-copy `isNull`/text path is built on. Per the ADR: "K1/K3 failure with a
*diagnosed* cause in the model (not the boundary) is a design fix, not a kill."

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
intended home for this and is being arranged separately. Until then, the
evidence for K5 is: the RAII handles in `ReldexHandles.h` (structurally
exactly-once release), the 12,000-iteration teardown test above, and the flat
RSS across it. `reldex.h` offers no create/release counter to assert against,
so nothing stronger is claimed.

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
adds a `POST_BUILD` step doing
`cmake -E copy_if_different $<TARGET_FILE:reldex_ffi-shared> $<TARGET_FILE_DIR:target>`;
it is called once each for `Reldex` and `tst_coreinfo`. Qt's own DLLs are
resolved via `PATH` (`tools/dev-env/env.sh` puts Qt's `bin/` there); no
`windeployqt` step exists yet because this is a dev-build skeleton, not
packaging (that is a later M6 task).

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

- **Linux/macOS are untested.** The CMake is written to work there (Qt
  discovered via `CMAKE_PREFIX_PATH`/`Qt6_DIR`, Corrosion is
  platform-agnostic, `ui/build.sh` has a non-Windows branch), but nobody
  has run it — only a Windows 11 + MSVC 2022 + Qt 6.8.3 machine was
  available. CI for all three OSes is M6.7, not this task.
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
