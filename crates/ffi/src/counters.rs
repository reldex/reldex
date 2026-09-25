//! Live-object accounting, so a leak at the boundary is a failing assertion
//! instead of a hunch.
//!
//! # Why this exists
//!
//! ADR-0003 D2 plans ASan for the C smoke harness, and spike criterion K5
//! ("destroy the C++ bridge under a flood of completions, 10,000 iterations,
//! ASan — any use-after-free, race, or hang") is stated in terms of it. ASan
//! is not available on the dev machine, and a sanitizer would not catch the
//! failure anyone is actually likely to write anyway: not a use-after-free,
//! but a batch or an error the adapter forgot to release. That leak is
//! invisible to every test until memory runs out.
//!
//! Counting live caller-owned objects turns it into an assertion a test can
//! make in one line: run a whole lifecycle, then check the numbers are back
//! where they started.
//!
//! # Cost, and why it is always on
//!
//! One relaxed atomic increment when an object is created and one relaxed
//! decrement when it is dropped. Nothing on the per-cell path — the formatter
//! creates no objects at all — and nothing per row or per event. Measured: it
//! does not move the allocation count (it allocates nothing) or the
//! nanoseconds-per-cell figures.
//!
//! It is compiled unconditionally, never behind a feature, because ADR-0003
//! A8 fixes the rule that a cargo feature must not change the ABI: a
//! diagnostic that exists only in some builds is one the adapter cannot call.
//!
//! # What "live" means
//!
//! An object is live from the moment this library creates it until the moment
//! it is dropped — which for a caller-owned object is when the caller releases
//! it, and for an object still sitting inside an undrained event is when the
//! hub's last reference goes away. A batch queued in an event the adapter
//! never took is therefore **live**, which is the honest answer: its rows are
//! still in memory.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::status::{ReldexStatus, entry};
use crate::strings::{CStruct, write_out_struct};

/// One counter per kind of object the boundary hands out or holds.
///
/// Process-wide, not per hub: they are for tests and diagnostics, and a test
/// that creates two hubs wants to see both.
static HUBS: AtomicUsize = AtomicUsize::new(0);
static SESSIONS: AtomicUsize = AtomicUsize::new(0);
static BATCHES: AtomicUsize = AtomicUsize::new(0);
static ERRORS: AtomicUsize = AtomicUsize::new(0);
static ARENAS: AtomicUsize = AtomicUsize::new(0);
/// Every owned, caller-released object M2.11 added — server output line
/// sets, metadata queries, profiles/profile lists, history pages and
/// worksheets/worksheet lists — share one counter rather than one field each:
/// each is small, released promptly, and the point of this diagnostic is
/// "did everything handed out come back", not a breakdown per family. A
/// family that turns out to need its own count can move to a dedicated field
/// later (additive, per D7).
static MISC_OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Which counter an object belongs to.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Hub,
    Session,
    Batch,
    Error,
    Arena,
    /// See [`MISC_OBJECTS`].
    ServerOutputLines,
    /// See [`MISC_OBJECTS`].
    WorkspaceObject,
}

impl Kind {
    const fn counter(self) -> &'static AtomicUsize {
        match self {
            Self::Hub => &HUBS,
            Self::Session => &SESSIONS,
            Self::Batch => &BATCHES,
            Self::Error => &ERRORS,
            Self::Arena => &ARENAS,
            Self::ServerOutputLines | Self::WorkspaceObject => &MISC_OBJECTS,
        }
    }
}

/// Records that one object of `kind` now exists.
pub(crate) fn created(kind: Kind) {
    kind.counter().fetch_add(1, Ordering::Relaxed);
}

/// Records that one object of `kind` has been destroyed.
pub(crate) fn destroyed(kind: Kind) {
    let previous = kind.counter().fetch_sub(1, Ordering::Relaxed);
    // A release that was not paired with a create would wrap the count to
    // `SIZE_MAX` and leave every later reading nonsense — a leak check that
    // reports an absurd number instead of the bug that caused it. In a debug
    // build, name it where it happens.
    debug_assert!(
        previous > 0,
        "reldex-ffi: an object was destroyed more times than it was created"
    );
}

/// How many objects of each kind this process currently holds.
///
/// **A diagnostic, not part of the working API.** It exists so tests and a
/// leak check can assert that everything handed out has come back; an adapter
/// has no reason to call it outside its own test suite.
///
/// Like every other exported function, it is refused from inside a waker
/// callback (ADR-0003 D5 rule 1): it reports `RELDEX_STATUS_REENTRANT`, writes
/// nothing to `out`, and records the usual last error. A diagnostic is no
/// reason to make an exception to the rule it would be used to debug.
///
/// Counts are process-wide and include objects that are not the caller's yet:
/// a `ReldexBatch` sitting inside an event nobody has drained is **live**,
/// because its rows are still in memory. So is the `ReldexError` in a thread's
/// last-error slot, until it is taken or cleared. A session stays live until
/// its pump thread has finished, which after `reldex_hub_destroy` may be a
/// while if it is parked inside an uninterruptible statement (ADR-0003 A17) —
/// that is exactly the leak this is meant to make visible.
///
/// # Safety
///
/// `out` must be null, or point at a writable [`ReldexLiveCounts`] with its
/// `struct_size` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_live_counts(out: *mut ReldexLiveCounts) -> ReldexStatus {
    entry(|| {
        let counts = ReldexLiveCounts {
            struct_size: u32::try_from(size_of::<ReldexLiveCounts>()).unwrap_or(u32::MAX),
            hubs: HUBS.load(Ordering::Relaxed),
            sessions: SESSIONS.load(Ordering::Relaxed),
            batches: BATCHES.load(Ordering::Relaxed),
            errors: ERRORS.load(Ordering::Relaxed),
            arenas: ARENAS.load(Ordering::Relaxed),
            misc_objects: MISC_OBJECTS.load(Ordering::Relaxed),
        };
        // SAFETY: delegated to this function's contract for `out`.
        if unsafe { write_out_struct(out, counts) } {
            ReldexStatus::Ok
        } else {
            crate::error::set_last_argument_error(
                "reldex_live_counts: `out` is null, unaligned, or too small",
            )
        }
    })
}

/// How many objects of each kind are live; see [`reldex_live_counts`].
///
/// Every count is a `size_t`, per the ADR-0003 A11 width rule: these are
/// counts of things in this process.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReldexLiveCounts {
    /// `sizeof(ReldexLiveCounts)` on the way in; how much is valid on the way
    /// out.
    pub struct_size: u32,
    /// Hubs created by `reldex_hub_create` and not yet fully torn down. A hub
    /// outlives `reldex_hub_destroy` until its last session pump exits.
    pub hubs: usize,
    /// Sessions whose registry entry or pump thread is still alive.
    pub sessions: usize,
    /// Fetched batches: held by the caller, or queued in an undrained event.
    pub batches: usize,
    /// Error objects: held by the caller, queued in an undrained event, or
    /// sitting in some thread's last-error slot.
    pub errors: usize,
    /// Text arenas created by `reldex_text_arena_create`.
    pub arenas: usize,
    /// Everything else M2.11 added that the caller owns and releases: server
    /// output line sets, metadata queries, profiles and profile lists,
    /// history pages, and worksheets and worksheet lists.
    pub misc_objects: usize,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, every other field a `usize` —
// valid when zeroed.
unsafe impl CStruct for ReldexLiveCounts {
    // The size **before** M2.11 added `misc_objects`: an older caller's
    // struct (compiled against a header that predates that field) must still
    // be accepted, per the D7 prefix rule that lets fields be appended
    // without breaking older callers. `offset_of!` gives that old size
    // exactly and portably (unlike a hard-coded byte count, which the
    // pre-M2.11 struct's platform-dependent padding would make wrong on at
    // least one target): `misc_objects` is the newest field, appended last,
    // so its offset in the current layout equals the old struct's size.
    const MIN_SIZE: usize = std::mem::offset_of!(Self, misc_objects);
}

impl Default for ReldexLiveCounts {
    /// Zeroed with `struct_size` set, which is the shape a caller passes in.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            hubs: 0,
            sessions: 0,
            batches: 0,
            errors: 0,
            arenas: 0,
            misc_objects: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, ReldexLiveCounts, created, destroyed, reldex_live_counts};
    use crate::status::ReldexStatus;

    fn read() -> ReldexLiveCounts {
        let mut counts = ReldexLiveCounts::default();
        // SAFETY: `counts` is a real local with `struct_size` set.
        let status = unsafe { reldex_live_counts(std::ptr::from_mut(&mut counts)) };
        assert_eq!(status, ReldexStatus::Ok);
        counts
    }

    #[test]
    fn a_created_object_is_counted_until_it_is_destroyed() {
        let before = read().arenas;
        created(Kind::Arena);
        assert_eq!(read().arenas, before + 1);
        destroyed(Kind::Arena);
        assert_eq!(read().arenas, before);
    }

    #[test]
    fn a_null_out_struct_is_refused_rather_than_dereferenced() {
        // SAFETY: a null pointer is exactly what this must refuse.
        let status = unsafe { reldex_live_counts(std::ptr::null_mut()) };
        assert_eq!(status, ReldexStatus::InvalidArgument);
        crate::error::take_last_error();
    }
}
