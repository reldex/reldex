//! The bulk formatter's allocation budget, measured rather than asserted from
//! intuition (`AGENTS.md`, "Performance").
//!
//! Its own test binary because a `#[global_allocator]` is per-binary: this one
//! counts every allocation the process makes, which would be noise anywhere
//! else.
//!
//! The claim under test is **not** "formatting is fast" — nothing here asserts
//! a duration, and a loaded CI runner cannot fail it. The claim is *zero heap
//! allocations per cell once the arena has capacity*, which is deterministic:
//! the arena's buffer, its offset vector and its two scratch buffers all keep
//! their capacity across `reldex_text_arena_clear`, so a second identical
//! window must allocate nothing at all.
//!
//! Spike S15's K4 budgets 200 ns per cell. An allocation and its matching free
//! are a meaningful fraction of that, and `NUMBER`/`TIMESTAMP` — which used to
//! cost 3 and 4 allocations per cell respectively — are what a grid of
//! financial data is mostly made of.

#![allow(
    unsafe_code,
    reason = "these tests drive the crate's own C ABI, which is what they exist to prove"
)]

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use reldex_ffi::{
    ReldexColumnView, ReldexEventKind, ReldexFormatOptions, ReldexMockScenarioConfig,
    ReldexMockStatement, ReldexStatus, reldex_batch_column, reldex_batch_column_fixed,
    reldex_batch_format_column, reldex_hub_pending_events, reldex_text_arena_clear,
    reldex_text_arena_count, reldex_text_arena_create, reldex_text_arena_release,
};

use support::{Harness, OwnedBatch};

/// Counts allocations while armed. Disarmed by default so the harness, the
/// test framework and the mock driver are not measured.
struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// Bytes allocated and not yet freed, tracked unconditionally so a *retention*
/// measurement can take a difference across an operation. Signed, because a
/// free of something allocated before the process reached this counter is
/// normal and must not wrap.
static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);

// SAFETY: every method forwards to `System` unchanged; the only addition is a
// relaxed counter bump, which allocates nothing itself.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
        // SAFETY: delegated to the caller of `alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        // SAFETY: delegated to the caller of `dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        LIVE_BYTES.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        // SAFETY: delegated to the caller of `realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        LIVE_BYTES.fetch_add(layout.size() as i64, Ordering::Relaxed);
        // SAFETY: delegated to the caller of `alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Serialises the tests in this binary.
///
/// `ARMED` and `ALLOCATIONS` are process-wide by construction — a
/// `#[global_allocator]` has no other shape — so two armed tests running at
/// once measure each other. Taking this first is what makes each measurement
/// the operation's own cost.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn exclusively() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Asserts the only other thread that could allocate into this count is idle.
///
/// The counter is process-wide, and a session's worker thread allocates while
/// it runs a request and builds its events. Arming while one is still working
/// would measure it too — a flaky non-zero that has nothing to do with the
/// call under test. Every request this file submits has been answered and
/// drained by the time it measures, so the worker is parked waiting for its
/// next command; this states that precondition instead of assuming it.
fn assert_the_worker_is_parked(harness: &Harness) {
    // SAFETY: the hub is live for the harness's lifetime.
    let pending = unsafe { reldex_hub_pending_events(harness.hub()) };
    assert_eq!(
        pending, 0,
        "an undrained event means the worker has been working; nothing may be measured until it \
         is parked"
    );
    assert!(
        harness.poll_event().is_none(),
        "the queue must be empty before the allocation counter is armed"
    );
}

/// Runs `body` with the counter armed and reports what it cost.
fn allocations_during(body: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    body();
    ARMED.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst)
}

/// Bytes allocated and not yet freed, right now.
fn live_bytes() -> i64 {
    LIVE_BYTES.load(Ordering::SeqCst)
}

const ROWS: usize = 200;

fn config() -> ReldexMockScenarioConfig {
    ReldexMockScenarioConfig {
        rows: ROWS as u64,
        seed: 11,
        ..ReldexMockScenarioConfig::default()
    }
}

#[test]
fn formatting_a_warm_window_allocates_nothing() {
    let _guard = exclusively();
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert!(executed.error.is_null());
    let result = executed.result;

    assert_eq!(
        harness.fetch(session, 20, result, ROWS as u32),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
    let batch = OwnedBatch(event.batch);
    assert!(!batch.0.is_null());

    let arena = reldex_text_arena_create();
    assert!(!arena.is_null());
    let options = ReldexFormatOptions {
        // Grouping and a fraction limit on purpose: they are the branches that
        // used to build intermediate strings.
        grouping_separator: u32::from(','),
        grouping_size: 3,
        max_fraction_digits: 2,
        ..ReldexFormatOptions::default()
    };

    // One column of each interesting kind: NUMBER (mirrored + rounded),
    // VARCHAR2 (borrowed) and DATE (rendered through Display).
    let format_window = || {
        // SAFETY: the batch, the arena and `options` are all live locals.
        unsafe {
            reldex_text_arena_clear(arena);
            for column in 0..3 {
                let status = reldex_batch_format_column(
                    batch.0,
                    column,
                    0,
                    ROWS,
                    std::ptr::from_ref(&options),
                    arena,
                );
                assert_eq!(status, ReldexStatus::Ok);
            }
        }
    };

    // Warm-up: this one is allowed to allocate, because the arena starts with
    // no capacity and the offsets vector has to grow. Amortised growth is
    // exactly what this pass excludes.
    assert_the_worker_is_parked(&harness);
    let warm_up = allocations_during(format_window);
    // SAFETY: the arena is live.
    assert_eq!(
        unsafe { reldex_text_arena_count(arena) },
        ROWS * 3,
        "one string per cell"
    );

    let steady = allocations_during(format_window);
    assert_eq!(
        steady,
        0,
        "formatting {} cells into a warm arena allocated {steady} times (warm-up cost {warm_up}); \
         the per-cell path must not allocate",
        ROWS * 3
    );

    // A third pass, to rule out a cache that only survives one round.
    let again = allocations_during(format_window);
    assert_eq!(again, 0);

    // SAFETY: the arena came from this library and is released once.
    unsafe { reldex_text_arena_release(arena) };
}

/// Prints the per-cell cost. Ignored by default: it is a measurement, not an
/// assertion, and a loaded machine must never fail the suite. Run it with
/// `cargo test --release -p reldex-ffi --test allocations -- --ignored
/// --nocapture`.
#[test]
#[ignore = "a measurement for spike S15's K4, not a pass/fail check"]
fn measure_the_cost_per_cell() {
    let _guard = exclusively();
    let harness = Harness::new();
    let session = harness.open(ReldexMockScenarioConfig {
        rows: 5_000,
        seed: 11,
        ..ReldexMockScenarioConfig::default()
    });
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    let result = executed.result;
    assert_eq!(harness.fetch(session, 20, result, 5_000), ReldexStatus::Ok);
    let event = harness.next_event();
    let batch = OwnedBatch(event.batch);
    let rows = 5_000_usize;

    let arena = reldex_text_arena_create();
    let options = ReldexFormatOptions::default();
    let names = ["NUMBER", "VARCHAR2", "DATE"];
    for (column, name) in names.iter().enumerate() {
        // Warm up, then measure.
        for _ in 0..2 {
            // SAFETY: the batch and arena are live.
            unsafe {
                reldex_text_arena_clear(arena);
                reldex_batch_format_column(
                    batch.0,
                    column,
                    0,
                    rows,
                    std::ptr::from_ref(&options),
                    arena,
                );
            }
        }
        let started = std::time::Instant::now();
        const PASSES: u32 = 20;
        for _ in 0..PASSES {
            // SAFETY: as above.
            unsafe {
                reldex_text_arena_clear(arena);
                reldex_batch_format_column(
                    batch.0,
                    column,
                    0,
                    rows,
                    std::ptr::from_ref(&options),
                    arena,
                );
            }
        }
        let elapsed = started.elapsed();
        let per_cell = elapsed.as_nanos() as f64 / f64::from(PASSES) / rows as f64;
        // The measurement's whole output. `--nocapture` shows it; clippy's
        // no-printing rule is for library code, not for a reporting test.
        #[expect(clippy::print_stdout, reason = "this test exists to print a number")]
        {
            println!("{name}: {per_cell:.1} ns/cell");
        }
    }
    // SAFETY: the arena came from this library and is released once.
    unsafe { reldex_text_arena_release(arena) };
}

/// The claim behind ADR-0003 amendment A19: describing a column costs nothing.
///
/// `reldex_batch_column` used to build a `ReldexNumber`/`ReldexTimestamp`
/// mirror for the whole column on first sight — 46 and 16 bytes per row,
/// retained for the batch's life — whether or not the caller ever looked at
/// `fixed`. The Qt model never does: it reads `null_bits`, the borrowed UTF-8
/// of a text column, or the bulk formatter's arena. So the view now allocates
/// nothing, and the mirror has its own call.
#[test]
fn describing_every_column_allocates_nothing() {
    let _guard = exclusively();
    let harness = Harness::new();
    let session = harness.open(config());
    assert_eq!(
        harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
        ReldexStatus::Ok
    );
    let executed = harness.next_event();
    assert!(executed.error.is_null());
    let result = executed.result;
    assert_eq!(
        harness.fetch(session, 20, result, ROWS as u32),
        ReldexStatus::Ok
    );
    let event = harness.next_event();
    assert_eq!(event.kind, ReldexEventKind::Fetched as i32);
    let batch = OwnedBatch(event.batch);
    assert!(!batch.0.is_null());

    let view = |column: usize| {
        let mut view = ReldexColumnView::default();
        // SAFETY: the batch is live and `view` is a local.
        let status = unsafe { reldex_batch_column(batch.0, column, std::ptr::from_mut(&mut view)) };
        assert_eq!(status, ReldexStatus::Ok);
        view
    };
    let describe_all = || {
        for column in 0..3 {
            let _ = view(column);
        }
    };

    // Warm-up, un-armed: a thread-local's first touch is not the path under
    // test.
    describe_all();

    assert_the_worker_is_parked(&harness);
    let first = allocations_during(describe_all);
    assert_eq!(
        first, 0,
        "the first description of every column allocated {first} times; NUMBER and TIMESTAMP must \
         not build a mirror here"
    );
    let again = allocations_during(describe_all);
    assert_eq!(again, 0);

    assert!(view(0).fixed.is_null(), "NUMBER exposes no mirror");
    assert!(view(2).fixed.is_null(), "TIMESTAMP exposes no mirror");
    assert_eq!(
        view(0).fixed_stride,
        size_of::<reldex_ffi::ReldexNumber>(),
        "the stride is still reported, so a caller can size its own buffer"
    );

    // And the mirror, when actually asked for, is built once and cached.
    let fixed = |column: usize| {
        let mut view = ReldexColumnView::default();
        // SAFETY: as above.
        let status =
            unsafe { reldex_batch_column_fixed(batch.0, column, std::ptr::from_mut(&mut view)) };
        assert_eq!(status, ReldexStatus::Ok);
        view
    };
    let built = allocations_during(|| {
        assert!(!fixed(0).fixed.is_null());
    });
    assert_eq!(
        built, 2,
        "the batch's mirror table (built on the first ask since M2.15, not per batch), then one          boxed slice for the whole column"
    );
    let cached = allocations_during(|| {
        assert!(!fixed(0).fixed.is_null());
    });
    assert_eq!(cached, 0, "the second ask is the cache");
    let second = allocations_during(|| {
        assert!(!fixed(2).fixed.is_null());
    });
    assert_eq!(
        second, 1,
        "a second column's mirror is its own slice, and nothing else"
    );
}

/// Prints the bytes a retained result costs per row, with and without the
/// fixed-element mirrors. Ignored by default: it is a measurement, and it
/// holds a million rows in memory while it runs.
///
/// **What the absolute numbers are, exactly.** They are whole-process live
/// bytes — everything allocated and not yet freed, including the mock driver's
/// generation buffers and whatever the session's worker happens to be holding — so
/// they are an upper bound on the boundary's own retention, not a measurement
/// of it, and they are not RSS either. The robust figure is the **difference**
/// between the two runs: the two differ in exactly one thing, so the delta is
/// the mirrors and nothing else. Quote the delta; treat the absolutes as
/// context.
///
/// This is the measurement behind spike S15's K3 (200 MB of RSS growth for 1M
/// rows of the S14 shape), which S15 itself will measure properly, as RSS. Run
/// it with
/// `cargo test --release -p reldex-ffi --test allocations -- --ignored
/// --nocapture measure_the_bytes`.
#[test]
#[ignore = "a measurement for spike S15's K3, not a pass/fail check"]
fn measure_the_bytes_retained_per_row() {
    let _guard = exclusively();
    const TOTAL: u64 = 1_000_000;
    const PER_FETCH: u32 = 50_000;

    for mirrors in [false, true] {
        let before = live_bytes();
        {
            let harness = Harness::new();
            let session = harness.open(ReldexMockScenarioConfig {
                rows: TOTAL,
                seed: 11,
                ..ReldexMockScenarioConfig::default()
            });
            assert_eq!(
                harness.execute(session, 10, ReldexMockStatement::GeneratedQuery),
                ReldexStatus::Ok
            );
            let result = harness.next_event().result;

            // Every batch retained at once, which is the worst case a grid
            // that never releases anything produces.
            let mut held = Vec::new();
            let mut request = 20;
            loop {
                assert_eq!(
                    harness.fetch(session, request, result, PER_FETCH),
                    ReldexStatus::Ok
                );
                request += 1;
                let event = harness.next_event();
                let batch = OwnedBatch(event.batch);
                if event.row_count == 0 {
                    break;
                }
                for column in 0..3 {
                    let mut view = ReldexColumnView::default();
                    let out = std::ptr::from_mut(&mut view);
                    // SAFETY: the batch is live and `view` is a local.
                    let status = unsafe {
                        if mirrors {
                            reldex_batch_column_fixed(batch.0, column, out)
                        } else {
                            reldex_batch_column(batch.0, column, out)
                        }
                    };
                    assert_eq!(status, ReldexStatus::Ok);
                }
                held.push(batch);
            }

            let peak = live_bytes() - before;
            let per_row = peak as f64 / TOTAL as f64;
            let what = if mirrors {
                "every column viewed WITH its fixed mirror"
            } else {
                "every column described, no mirror"
            };
            // The measurement's whole output; clippy's no-printing rule is for
            // library code, not for a reporting test.
            #[expect(clippy::print_stdout, reason = "this test exists to print a number")]
            {
                println!(
                    "{what}: {peak} bytes retained for {TOTAL} rows = {per_row:.1} B/row \
                     ({:.0} MB)",
                    peak as f64 / (1024.0 * 1024.0)
                );
            }
        }
    }
}
