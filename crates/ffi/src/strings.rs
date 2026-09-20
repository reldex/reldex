//! Borrowed strings, the `struct_size` prefix rule, and the scalar→UTF-16
//! offset conversion the adapter must not guess at (ADR-0003 D4/D7).

use std::mem::MaybeUninit;

use crate::status::entry_value;

/// A borrowed UTF-8 string: pointer plus length in **bytes**.
///
/// The promise depends on which way it is going, and the difference is the
/// contract, not an accident:
///
/// * **Out of Reldex** — every `ReldexStr` this library hands back (an error's
///   message, a column's name, a mock statement) is **NUL-terminated at
///   `ptr[len]`**, so `printf("%s")` and `QString::fromUtf8(s.ptr)` are both
///   safe. `len` is still authoritative: a string that *contains* a NUL byte
///   is `len` bytes long regardless.
/// * **Into Reldex** — a `ReldexStr` the caller builds needs only `len`
///   readable bytes. Nothing here reads `ptr[len]`, so a pointer into the
///   middle of a larger buffer is fine.
///
/// Never owned by the caller: an outbound one borrows from whatever produced
/// it (an error, a batch, an arena) and dies with it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexStr {
    /// First byte. Never null on the way out: an empty string points at a NUL
    /// byte.
    pub ptr: *const u8,
    /// Length in bytes, excluding the trailing NUL.
    pub len: usize,
}

/// One readable NUL byte, so an empty outbound string still has something to
/// point at and `ptr[0] == 0` holds.
pub(crate) static EMPTY_NUL: &[u8] = b"\0";

impl ReldexStr {
    /// The empty string.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            ptr: EMPTY_NUL.as_ptr(),
            len: 0,
        }
    }

    /// Borrows `text`, which must already be NUL-terminated at `text[len]`.
    ///
    /// This is the **only** constructor for an outbound string, so the
    /// NUL-termination promise above cannot be broken by accident: anything
    /// that is not already terminated has to be copied into an [`OwnedStr`]
    /// first, and that is a visible decision at the call site.
    pub(crate) fn borrow_nul_terminated(text: &str, len: usize) -> Self {
        debug_assert_eq!(
            text.as_bytes().get(len).copied(),
            Some(0),
            "an outbound ReldexStr must be NUL-terminated at ptr[len]"
        );
        Self {
            ptr: text.as_ptr(),
            len,
        }
    }

    /// Reads the string back.
    ///
    /// Rust-side convenience — a C caller uses `ptr` and `len` directly. The
    /// returned borrow is unbounded, so it must not outlive whatever owns the
    /// bytes.
    ///
    /// # Safety
    ///
    /// `self` must be a `ReldexStr` this library produced, or a caller-built
    /// one whose `ptr` points at `len` readable, initialized bytes.
    #[must_use]
    pub unsafe fn as_bytes<'a>(self) -> Option<&'a [u8]> {
        if self.len == 0 {
            return Some(&[]);
        }
        if self.ptr.is_null() {
            return None;
        }
        // SAFETY: the caller promises `ptr` covers `len` initialized bytes and
        // outlives `'a`; `len != 0` was checked, so the slice is non-empty and
        // the pointer is not the dangling empty-slice case.
        Some(unsafe { std::slice::from_raw_parts(self.ptr, self.len) })
    }

    /// Reads the string back as UTF-8.
    ///
    /// # Safety
    ///
    /// As [`ReldexStr::as_bytes`].
    #[must_use]
    pub unsafe fn as_str<'a>(self) -> Option<&'a str> {
        // SAFETY: delegated to the caller's promise, unchanged.
        let bytes = unsafe { self.as_bytes() }?;
        std::str::from_utf8(bytes).ok()
    }
}

/// An owned string kept for as long as the object that carries it, stored with
/// a trailing NUL so a [`ReldexStr`] into it keeps the outbound promise.
///
/// Copying is the point. Rust's `String`/`Box<str>` are *not* NUL-terminated,
/// and `ptr[len]` on one is a byte past the allocation — reading it is
/// undefined behaviour no matter what it happens to contain. Anything Reldex
/// hands to C as a string therefore gets copied here once, at a point where
/// the copy is amortised over many reads (per error, per result set), never
/// per cell.
#[derive(Debug)]
pub(crate) struct OwnedStr {
    text: String,
    len: usize,
}

impl OwnedStr {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        let mut text = text.into();
        let len = text.len();
        text.push('\0');
        Self { text, len }
    }

    pub(crate) fn as_reldex_str(&self) -> ReldexStr {
        ReldexStr::borrow_nul_terminated(&self.text, self.len)
    }
}

/// A `#[repr(C)]` struct that follows the ADR-0003 D7 prefix rule.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]`, hold no padding-sensitive invariant, be
/// valid when every byte is zero (so a field an older caller did not supply
/// reads as "unknown/default"), and begin with a `uint32_t struct_size`.
pub(crate) unsafe trait CStruct: Copy {
    /// The smallest `struct_size` this build accepts. For a struct that has
    /// never had a field appended this is simply its current size.
    const MIN_SIZE: usize;
}

/// Reads a caller-supplied **input** struct, tolerating a `struct_size` from
/// an older *or* newer header: fields this build knows but the caller did not
/// supply read back as zero, which every input struct documents as "default".
///
/// # Safety
///
/// `ptr` must be null, or aligned for `T` and point at `struct_size` readable
/// initialized bytes.
pub(crate) unsafe fn read_in_struct<T: CStruct>(ptr: *const T) -> Option<T> {
    if ptr.is_null() || !ptr.is_aligned() {
        return None;
    }
    // SAFETY: `ptr` is non-null and aligned, and the caller promises at least
    // `struct_size` initialized bytes — which by the prefix rule always
    // includes the leading `u32` itself.
    let declared = unsafe { ptr.cast::<u32>().read() } as usize;
    if declared < T::MIN_SIZE {
        return None;
    }
    let take = declared.min(size_of::<T>());
    let mut value = MaybeUninit::<T>::zeroed();
    // SAFETY: `take <= size_of::<T>()` and `take <= declared`, so the read
    // stays inside the caller's struct and the write stays inside `value`.
    // Neither can overlap: `value` is a fresh local. `T: CStruct` promises a
    // zeroed `T` is valid, so the bytes `take` leaves untouched are sound.
    unsafe {
        std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), value.as_mut_ptr().cast::<u8>(), take);
    }
    // SAFETY: every byte is now either copied from the caller or zero, and
    // `T: CStruct` promises an all-zero `T` is a valid value.
    Some(unsafe { value.assume_init() })
}

/// Checks that a caller-supplied **output** struct is one this build can fill,
/// **without writing to it**.
///
/// Used where a refusal must leave `*ptr` untouched — [`crate::
/// reldex_hub_next_event`] validates before it pops an event, so a mis-sized
/// struct neither consumes the event nor scribbles on the caller's memory.
/// Writability itself cannot be checked from here; reading the leading `u32`
/// is the most this can prove, and the caller's contract covers the rest.
///
/// # Safety
///
/// `ptr` must be null, or aligned for `T` with its leading `u32` initialized
/// and readable.
pub(crate) unsafe fn check_out_struct<T: CStruct>(ptr: *const T) -> bool {
    if ptr.is_null() || !ptr.is_aligned() {
        return false;
    }
    // SAFETY: non-null and aligned; the caller promises the leading `u32` is
    // initialized, which the prefix rule puts inside every valid `struct_size`.
    let declared = unsafe { ptr.cast::<u32>().read() } as usize;
    declared >= T::MIN_SIZE
}

/// Fills a caller-supplied **output** struct, writing only as many bytes as
/// the caller's `struct_size` says it owns and reporting back how many of them
/// are valid.
///
/// The caller must initialize `struct_size` before the call
/// (`ReldexEvent ev = { .struct_size = sizeof ev };`). A `struct_size` below
/// [`CStruct::MIN_SIZE`] is refused rather than truncated, because silently
/// dropping a field that transfers ownership — an error or a batch pointer —
/// would leak it.
///
/// # Safety
///
/// `ptr` must be null, or aligned for `T` and point at `struct_size` writable
/// bytes, with its leading `u32` already initialized.
pub(crate) unsafe fn write_out_struct<T: CStruct>(ptr: *mut T, value: T) -> bool {
    if ptr.is_null() || !ptr.is_aligned() {
        return false;
    }
    // SAFETY: non-null and aligned; the caller promises the leading `u32` is
    // initialized and writable.
    let declared = unsafe { ptr.cast::<u32>().read() } as usize;
    if declared < T::MIN_SIZE {
        return false;
    }
    let take = declared.min(size_of::<T>());
    // SAFETY: `take` bytes fit inside both the caller's struct (`take <=
    // declared`) and `value` (`take <= size_of::<T>()`); `value` is a local, so
    // the ranges cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::addr_of!(value).cast::<u8>(),
            ptr.cast::<u8>(),
            take,
        );
    }
    let reported = u32::try_from(take).unwrap_or(u32::MAX);
    // SAFETY: the first four bytes are inside `take >= MIN_SIZE >= 4` bytes the
    // caller owns and are aligned, having just been read from the same place.
    unsafe {
        ptr.cast::<u32>().write(reported);
    }
    true
}

/// What [`reldex_utf16_offset`] returns when it cannot answer.
pub const RELDEX_UTF16_OFFSET_INVALID: usize = usize::MAX;

/// Converts a Unicode **scalar** offset into a UTF-16 code-unit offset.
///
/// `SqlPosition` counts scalars (ADR-0002 S3); `QString` counts UTF-16 units.
/// Thai is BMP and the two coincide there, but the non-BMP corpus of spike S11
/// proves they do not in general, so the adapter must never guess. An offset
/// past the end of `text` reports the string's full UTF-16 length, which is
/// where a caret belongs when the server points one character past the last.
///
/// Returns [`RELDEX_UTF16_OFFSET_INVALID`] if `text` is not valid UTF-8 or its
/// pointer is null.
///
/// # Safety
///
/// `text` must be a `ReldexStr` this library produced, or point at `len`
/// readable initialized bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_utf16_offset(text: ReldexStr, char_offset: usize) -> usize {
    entry_value(RELDEX_UTF16_OFFSET_INVALID, || {
        // SAFETY: delegated to this function's own safety contract.
        let Some(text) = (unsafe { text.as_str() }) else {
            return RELDEX_UTF16_OFFSET_INVALID;
        };
        let mut units = 0_usize;
        for (index, ch) in text.chars().enumerate() {
            if index == char_offset {
                return units;
            }
            units += ch.len_utf16();
        }
        units
    })
}

#[cfg(test)]
mod tests {
    use super::{CStruct, RELDEX_UTF16_OFFSET_INVALID, read_in_struct, reldex_utf16_offset};
    use crate::strings::{OwnedStr, ReldexStr, write_out_struct};

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Probe {
        struct_size: u32,
        first: u32,
        second: u32,
    }

    // SAFETY: `#[repr(C)]`, all-integer, valid when zeroed, `struct_size`
    // first.
    unsafe impl CStruct for Probe {
        const MIN_SIZE: usize = 8;
    }

    fn probe(struct_size: u32) -> Probe {
        Probe {
            struct_size,
            first: 11,
            second: 22,
        }
    }

    #[test]
    fn a_short_input_struct_reads_absent_fields_as_zero() {
        let value = probe(8);
        // SAFETY: `value` is a real `Probe` and outlives the call.
        let read = unsafe { read_in_struct(std::ptr::from_ref(&value)) }.expect("accepted");
        assert_eq!(read.first, 11);
        assert_eq!(read.second, 0, "a field the caller did not supply is zero");
    }

    #[test]
    fn a_long_input_struct_is_accepted_and_the_extra_ignored() {
        let value = probe(64);
        // SAFETY: as above; `read_in_struct` copies at most `size_of::<Probe>()`
        // bytes, so the oversized `struct_size` cannot make it read past
        // `value`.
        let read = unsafe { read_in_struct(std::ptr::from_ref(&value)) }.expect("accepted");
        assert_eq!((read.first, read.second), (11, 22));
    }

    #[test]
    fn an_input_struct_that_is_too_small_or_null_is_refused() {
        let value = probe(4);
        // SAFETY: `value` is a real `Probe`.
        assert!(unsafe { read_in_struct(std::ptr::from_ref(&value)) }.is_none());
        // SAFETY: a null pointer is explicitly allowed by the contract.
        assert!(unsafe { read_in_struct::<Probe>(std::ptr::null()) }.is_none());
    }

    #[test]
    fn an_output_struct_is_filled_only_as_far_as_the_caller_owns_it() {
        let mut out = probe(8);
        out.first = 0;
        out.second = 0;
        // SAFETY: `out` is a real `Probe` with `struct_size` initialized.
        assert!(unsafe { write_out_struct(std::ptr::from_mut(&mut out), probe(12)) });
        assert_eq!(out.struct_size, 8, "the caller is told what is valid");
        assert_eq!(out.first, 11);
        assert_eq!(out.second, 0, "past the caller's struct_size, untouched");

        let mut small = probe(4);
        // SAFETY: as above.
        assert!(!unsafe { write_out_struct(std::ptr::from_mut(&mut small), probe(12)) });
        // SAFETY: a null pointer is explicitly allowed by the contract.
        assert!(!unsafe { write_out_struct(std::ptr::null_mut(), probe(12)) });
    }

    #[test]
    fn utf16_offsets_follow_the_scalars_not_the_bytes() {
        let owned = OwnedStr::new("ก🚀b");
        let text = owned.as_reldex_str();
        // SAFETY: `owned` outlives the calls below.
        unsafe {
            assert_eq!(reldex_utf16_offset(text, 0), 0);
            assert_eq!(reldex_utf16_offset(text, 1), 1, "Thai is one UTF-16 unit");
            assert_eq!(reldex_utf16_offset(text, 2), 3, "the emoji is a pair");
            assert_eq!(reldex_utf16_offset(text, 3), 4);
            assert_eq!(reldex_utf16_offset(text, 99), 4, "past the end clamps");
        }

        let invalid = ReldexStr {
            ptr: b"\xff\xfe".as_ptr(),
            len: 2,
        };
        // SAFETY: the pointer covers two readable bytes; they are not UTF-8,
        // which is the case under test.
        let offset = unsafe { reldex_utf16_offset(invalid, 0) };
        assert_eq!(offset, RELDEX_UTF16_OFFSET_INVALID);
    }
}
