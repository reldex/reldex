//! Status codes, panic containment and the re-entrancy guard every exported
//! function goes through (ADR-0003 D2/D5).

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};

use crate::error::set_last_error;

/// What an exported function reports.
///
/// `0` is reserved for "a status this header predates", per ADR-0003 D7: an
/// adapter must treat an unmapped value as an unknown failure rather than
/// assert.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexStatus {
    /// A status value this header does not know. Never returned by this
    /// library; reserved so an older adapter can recognise a newer one.
    Unknown = 0,
    /// The call succeeded.
    Ok = 1,
    /// The call failed with a database or core error. Take the details with
    /// [`crate::reldex_last_error_take`].
    Error = 2,
    /// An argument was null, unaligned, out of range, or carried a
    /// `struct_size` this build cannot honour. Nothing was done.
    InvalidArgument = 3,
    /// The object is not in a state where this call makes sense — a session
    /// that is still opening, or one that is closed.
    InvalidState = 4,
    /// An id was not found: it belongs to another session, or it was already
    /// closed. Reported, never undefined behaviour (ADR-0003 D3).
    NotFound = 5,
    /// A panic was caught at the boundary. The process is intact and the call
    /// did nothing useful; the panic message is in the thread-local last
    /// error. This is a bug in Reldex, not an expected outcome.
    Panic = 6,
    /// The call was made from inside a waker callback, which the D5 contract
    /// forbids. Nothing was done — the alternative is a deadlock.
    Reentrant = 7,
}

thread_local! {
    /// Set while this thread is inside a waker callback. See
    /// [`WakerGuard`].
    static IN_WAKER: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as "inside the waker" until dropped.
///
/// The guard is always on, not only in debug builds as ADR-0003 D2 first
/// sketched: it costs one thread-local bool read per FFI call — far below the
/// noise floor of the K4 per-batch budget, and it is what turns the classic
/// waker deadlock into a reported error.
pub(crate) struct WakerGuard;

impl WakerGuard {
    pub(crate) fn enter() -> Self {
        IN_WAKER.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for WakerGuard {
    fn drop(&mut self) {
        IN_WAKER.with(|flag| flag.set(false));
    }
}

fn in_waker() -> bool {
    IN_WAKER.with(Cell::get)
}

fn reentrancy_error() -> reldex_db_core::DbError {
    reldex_db_core::DbError::internal(
        "reldex-ffi: an FFI function was called from inside a waker callback, which the \
         ADR-0003 D5 contract forbids; the waker must only post to the adapter's event loop",
    )
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic payload".to_owned())
}

/// Runs the body of an exported function that reports a [`ReldexStatus`].
///
/// Two things happen here and nowhere else: a call made from inside a waker is
/// refused rather than allowed to deadlock, and a panic is contained instead of
/// unwinding across the C ABI (which is undefined behaviour). Honest caveat,
/// from ADR-0003 D2: `oracledb` 26.0.0-beta.3 can *abort* the process while
/// unwinding, and no wrapper here can contain that.
pub(crate) fn entry(body: impl FnOnce() -> ReldexStatus) -> ReldexStatus {
    if in_waker() {
        set_last_error(reentrancy_error());
        return ReldexStatus::Reentrant;
    }
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
            set_last_error(reldex_db_core::DbError::internal(format!(
                "reldex-ffi: a panic was caught at the FFI boundary: {}",
                panic_message(payload.as_ref())
            )));
            ReldexStatus::Panic
        }
    }
}

/// Runs the body of an exported function that reports something other than a
/// [`ReldexStatus`], falling back to `fallback` on a panic or a re-entrant
/// call.
pub(crate) fn entry_value<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    if in_waker() {
        set_last_error(reentrancy_error());
        return fallback;
    }
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            set_last_error(reldex_db_core::DbError::internal(format!(
                "reldex-ffi: a panic was caught at the FFI boundary: {}",
                panic_message(payload.as_ref())
            )));
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReldexStatus, WakerGuard, entry, entry_value};
    use crate::error::take_last_error;

    #[test]
    fn a_panic_becomes_a_status_not_an_unwind() {
        let status = entry(|| panic!("boom"));
        assert_eq!(status, ReldexStatus::Panic);
        let error = take_last_error().expect("a caught panic records a last error");
        assert!(error.message().contains("boom"), "{}", error.message());

        assert!(!entry_value(false, || panic!("boom")));
        assert!(take_last_error().is_some());
    }

    #[test]
    fn an_entry_from_inside_a_waker_is_refused_rather_than_deadlocking() {
        let _guard = WakerGuard::enter();
        assert_eq!(entry(|| ReldexStatus::Ok), ReldexStatus::Reentrant);
        assert_eq!(entry_value(7_u32, || 1_u32), 7);
        let error = take_last_error().expect("a refused call records why");
        assert!(error.message().contains("waker"), "{}", error.message());
    }

    #[test]
    fn the_guard_clears_when_the_waker_returns() {
        {
            let _guard = WakerGuard::enter();
        }
        assert_eq!(entry(|| ReldexStatus::Ok), ReldexStatus::Ok);
    }
}
