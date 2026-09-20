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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use reldex_ffi::{
    ReldexEventKind, ReldexFormatOptions, ReldexMockScenarioConfig, ReldexMockStatement,
    ReldexStatus, reldex_batch_format_column, reldex_text_arena_clear, reldex_text_arena_count,
    reldex_text_arena_create, reldex_text_arena_release,
};

use support::{Harness, OwnedBatch};

/// Counts allocations while armed. Disarmed by default so the harness, the
/// test framework and the mock driver are not measured.
struct Counting;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards to `System` unchanged; the only addition is a
// relaxed counter bump, which allocates nothing itself.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: delegated to the caller of `alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: delegated to the caller of `dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: delegated to the caller of `realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: delegated to the caller of `alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `body` with the counter armed and reports what it cost.
fn allocations_during(body: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    body();
    ARMED.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst)
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
