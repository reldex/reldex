//! The error model's crossing (ADR-0003 D6): an owned `ReldexError*`, a
//! borrowed [`ReldexErrorView`] onto it, and the thread-local last error that
//! functions returning a bare status record their failure in.

use std::cell::RefCell;
use std::error::Error as _;

use reldex_db_core::{DbError, ErrorKind, SessionState};
use reldex_db_driver_api::SqlPosition;

use crate::status::{ReldexStatus, entry, entry_value};
use crate::strings::{CStruct, OwnedStr, ReldexStr, write_out_struct};

/// Why an operation failed, as a vendor-neutral category.
///
/// `ErrorKind` is `#[non_exhaustive]` in Rust, so `0` is reserved here for a
/// kind this header predates: an adapter must map an unknown value to
/// "unknown" rather than assert (ADR-0003 D6). The native code in
/// [`ReldexErrorView::native_code`] carries the vendor's own classification
/// regardless.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexErrorKind {
    /// A kind this header does not know.
    Unknown = 0,
    /// Connection parameters are invalid or incomplete; nothing was attempted.
    Configuration = 1,
    /// A connection could not be established.
    Connection = 2,
    /// The server rejected the credentials or the authentication method.
    Authentication = 3,
    /// An established connection was lost mid-operation.
    NetworkLost = 4,
    /// An operation exceeded a configured time limit.
    Timeout = 5,
    /// The operation was cancelled.
    Cancelled = 6,
    /// The statement could not be parsed or compiled.
    Syntax = 7,
    /// A database constraint was violated.
    Constraint = 8,
    /// The session lacks privileges for the object or operation.
    Permission = 9,
    /// The operation failed because of transaction state.
    Transaction = 10,
    /// A server-side or client-side resource limit was reached.
    Resource = 11,
    /// A value could not be represented without loss, or was not valid for its
    /// declared type.
    DataConversion = 12,
    /// The driver does not implement the requested capability.
    Unsupported = 13,
    /// A Reldex or driver bug. An invalid FFI argument is reported as this.
    DriverInternal = 14,
    /// A database-reported error the driver could not classify.
    Other = 15,
}

impl From<ErrorKind> for ReldexErrorKind {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::Configuration => Self::Configuration,
            ErrorKind::Connection => Self::Connection,
            ErrorKind::Authentication => Self::Authentication,
            ErrorKind::NetworkLost => Self::NetworkLost,
            ErrorKind::Timeout => Self::Timeout,
            ErrorKind::Cancelled => Self::Cancelled,
            ErrorKind::Syntax => Self::Syntax,
            ErrorKind::Constraint => Self::Constraint,
            ErrorKind::Permission => Self::Permission,
            ErrorKind::Transaction => Self::Transaction,
            ErrorKind::Resource => Self::Resource,
            ErrorKind::DataConversion => Self::DataConversion,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::DriverInternal => Self::DriverInternal,
            ErrorKind::Other => Self::Other,
            // `ErrorKind` is `#[non_exhaustive]`: a kind added upstream
            // crosses as "unknown" rather than as a wrong category.
            _ => Self::Unknown,
        }
    }
}

/// What the driver believes about the session after a failure, plus the two
/// core-side terminal states.
///
/// `0` is reserved as unknown for the same reason as
/// [`ReldexErrorKind::Unknown`].
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReldexSessionState {
    /// A state this header does not know.
    Unknown = 0,
    /// The session is known to be usable; the failure was confined to the call.
    Usable = 1,
    /// The session may be usable; it will be validated before the next request.
    NeedsValidation = 2,
    /// The session is gone. Any transaction it held is lost.
    Lost = 3,
    /// The session was closed by the caller.
    Closed = 4,
}

impl From<SessionState> for ReldexSessionState {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Usable => Self::Usable,
            SessionState::NeedsValidation => Self::NeedsValidation,
            SessionState::Lost => Self::Lost,
        }
    }
}

impl From<reldex_db_core::SessionLifecycle> for ReldexSessionState {
    fn from(state: reldex_db_core::SessionLifecycle) -> Self {
        match state {
            reldex_db_core::SessionLifecycle::Usable => Self::Usable,
            reldex_db_core::SessionLifecycle::NeedsValidation => Self::NeedsValidation,
            reldex_db_core::SessionLifecycle::Lost => Self::Lost,
            reldex_db_core::SessionLifecycle::Closed => Self::Closed,
        }
    }
}

/// One failure, owned by the caller from the moment it is handed out.
///
/// Opaque: read it with [`reldex_error_view`], release it with
/// [`reldex_error_free`]. Every string in the view borrows from this object
/// and dies with it.
pub struct ReldexError {
    kind: ReldexErrorKind,
    session_state: ReldexSessionState,
    retryable: bool,
    native_code: Option<i32>,
    native_message: Option<OwnedStr>,
    position: Option<SqlPosition>,
    message: OwnedStr,
    cause: Option<OwnedStr>,
}

impl ReldexError {
    pub(crate) fn from_db_error(error: &DbError) -> Self {
        let mut cause = String::new();
        let mut source = error.source();
        while let Some(current) = source {
            if !cause.is_empty() {
                cause.push_str(": ");
            }
            cause.push_str(&current.to_string());
            source = current.source();
        }
        Self {
            kind: error.kind().into(),
            session_state: error.session_state().into(),
            retryable: error.is_retryable(),
            native_code: error.native().map(reldex_db_driver_api::NativeError::code),
            native_message: error
                .native()
                .map(|native| OwnedStr::new(native.message().to_owned())),
            position: error.position().copied(),
            message: OwnedStr::new(error.message().to_owned()),
            cause: (!cause.is_empty()).then(|| OwnedStr::new(cause)),
        }
    }

    /// The message, for this crate's own tests and logs.
    #[must_use]
    pub fn message(&self) -> &str {
        let view = self.message.as_reldex_str();
        // SAFETY: the string is owned by `self` and outlives the borrow.
        unsafe { view.as_str() }.unwrap_or_default()
    }

    /// The category, for this crate's own tests.
    #[must_use]
    pub const fn kind(&self) -> ReldexErrorKind {
        self.kind
    }

    /// What this failure says about the session, for the event that carries
    /// it.
    pub(crate) const fn session_state_for_event(&self) -> ReldexSessionState {
        self.session_state
    }

    fn view(&self) -> ReldexErrorView {
        ReldexErrorView {
            struct_size: u32::try_from(size_of::<ReldexErrorView>()).unwrap_or(u32::MAX),
            kind: self.kind as i32,
            session_state: self.session_state as i32,
            native_code: self.native_code.unwrap_or(0),
            char_offset: self
                .position
                .and_then(SqlPosition::char_offset)
                .unwrap_or(0),
            line: self.position.and_then(SqlPosition::line).unwrap_or(0),
            column: self.position.and_then(SqlPosition::column).unwrap_or(0),
            retryable: self.retryable,
            has_native: self.native_code.is_some(),
            has_char_offset: self
                .position
                .is_some_and(|position| position.char_offset().is_some()),
            has_line_column: self
                .position
                .is_some_and(|position| position.line().is_some()),
            message: self.message.as_reldex_str(),
            native_message: self
                .native_message
                .as_ref()
                .map_or_else(ReldexStr::empty, OwnedStr::as_reldex_str),
            cause: self
                .cause
                .as_ref()
                .map_or_else(ReldexStr::empty, OwnedStr::as_reldex_str),
        }
    }
}

/// A read-only description of a [`ReldexError`], with every string borrowed
/// from it.
///
/// The caller sets `struct_size` before the call; see
/// [`crate::reldex_error_view`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ReldexErrorView {
    /// `sizeof(ReldexErrorView)` on the way in; how much of it is valid on the
    /// way out.
    pub struct_size: u32,
    /// A [`ReldexErrorKind`].
    pub kind: i32,
    /// A [`ReldexSessionState`].
    pub session_state: i32,
    /// The vendor's numeric code (`ORA-00942` is `942`), when `has_native`.
    pub native_code: i32,
    /// Position in the statement text, counted in Unicode **scalars**, when
    /// `has_char_offset`. Convert with [`crate::reldex_utf16_offset`].
    pub char_offset: u32,
    /// 1-based line, when `has_line_column`.
    pub line: u32,
    /// 1-based column, when `has_line_column`.
    pub column: u32,
    /// Whether retrying the same operation could plausibly succeed.
    pub retryable: bool,
    /// Whether `native_code` and `native_message` carry a vendor error.
    pub has_native: bool,
    /// Whether `char_offset` is set.
    pub has_char_offset: bool,
    /// Whether `line` and `column` are set.
    pub has_line_column: bool,
    /// Reldex's own message.
    pub message: ReldexStr,
    /// The vendor's own message, unmodified; empty when `!has_native`.
    pub native_message: ReldexStr,
    /// The rendered `source` chain, or empty.
    pub cause: ReldexStr,
}

// SAFETY: `#[repr(C)]`, `struct_size` first, and every field is an integer,
// a `bool` (valid as 0) or a `ReldexStr` (two zeroable words); an all-zero
// value is a valid, if uninformative, view.
unsafe impl CStruct for ReldexErrorView {
    const MIN_SIZE: usize = size_of::<Self>();
}

impl Default for ReldexErrorView {
    /// An empty view with `struct_size` set, which is the shape a caller is
    /// expected to pass in. Rust-side convenience; a C caller writes
    /// `ReldexErrorView view = { .struct_size = sizeof view };`.
    fn default() -> Self {
        Self {
            struct_size: u32::try_from(size_of::<Self>()).unwrap_or(u32::MAX),
            kind: ReldexErrorKind::Unknown as i32,
            session_state: ReldexSessionState::Unknown as i32,
            native_code: 0,
            char_offset: 0,
            line: 0,
            column: 0,
            retryable: false,
            has_native: false,
            has_char_offset: false,
            has_line_column: false,
            message: ReldexStr::empty(),
            native_message: ReldexStr::empty(),
            cause: ReldexStr::empty(),
        }
    }
}

thread_local! {
    /// The last failure on this thread, for functions that return a bare
    /// status. Replaced, not accumulated: only the most recent failure is
    /// kept, exactly like `GetLastError`.
    static LAST_ERROR: RefCell<Option<Box<ReldexError>>> = const { RefCell::new(None) };
}

pub(crate) fn set_last_error(error: DbError) {
    set_last_reldex_error(ReldexError::from_db_error(&error));
}

pub(crate) fn set_last_reldex_error(error: ReldexError) {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(error));
    });
}

/// Records an argument failure, so a caller that got
/// `RELDEX_STATUS_INVALID_ARGUMENT` can still find out which argument.
pub(crate) fn set_last_argument_error(message: impl Into<String>) -> ReldexStatus {
    set_last_error(DbError::internal(format!("reldex-ffi: {}", message.into())));
    ReldexStatus::InvalidArgument
}

pub(crate) fn take_last_error() -> Option<Box<ReldexError>> {
    LAST_ERROR.with(|slot| slot.borrow_mut().take())
}

/// Takes the last failure recorded **on this thread**, transferring ownership.
///
/// Returns `NULL` when there is none. Release the result with
/// [`reldex_error_free`]. Calling this twice returns `NULL` the second time:
/// the error exists once.
#[unsafe(no_mangle)]
pub extern "C" fn reldex_last_error_take() -> *mut ReldexError {
    entry_value(std::ptr::null_mut(), || {
        take_last_error().map_or(std::ptr::null_mut(), Box::into_raw)
    })
}

/// Discards the last failure recorded on this thread, if any.
#[unsafe(no_mangle)]
pub extern "C" fn reldex_last_error_clear() {
    entry_value((), || {
        drop(take_last_error());
    });
}

/// Describes `error` into `out`.
///
/// # Safety
///
/// `error` must be a live pointer this library produced and has not yet been
/// freed. `out` must be non-null, aligned, and have its `struct_size` set to
/// the size of the caller's [`ReldexErrorView`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_error_view(
    error: *const ReldexError,
    out: *mut ReldexErrorView,
) -> ReldexStatus {
    entry(|| {
        if error.is_null() || !error.is_aligned() {
            return set_last_argument_error("reldex_error_view: `error` is null or unaligned");
        }
        // SAFETY: the caller promises `error` is a live `ReldexError` this
        // library produced; the borrow ends with this call, and the strings the
        // view borrows all live inside that object.
        let view = unsafe { &*error }.view();
        // SAFETY: delegated to this function's contract for `out`.
        if unsafe { write_out_struct(out, view) } {
            ReldexStatus::Ok
        } else {
            set_last_argument_error("reldex_error_view: `out` is null, unaligned, or too small")
        }
    })
}

/// Releases an error the caller was handed.
///
/// A null pointer is a no-op. Passing the same pointer twice is undefined
/// behaviour — like every `free`.
///
/// # Safety
///
/// `error` must be null, or a pointer this library produced and that has not
/// been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reldex_error_free(error: *mut ReldexError) {
    entry_value((), || {
        if error.is_null() {
            return;
        }
        // SAFETY: the caller promises this pointer came from `Box::into_raw` in
        // this library and has not been freed, so reconstructing the box and
        // dropping it is the matching deallocation.
        drop(unsafe { Box::from_raw(error) });
    });
}

#[cfg(test)]
mod tests {
    use super::{
        ReldexError, ReldexErrorKind, ReldexErrorView, ReldexSessionState, reldex_error_free,
        reldex_error_view, reldex_last_error_clear, reldex_last_error_take, set_last_error,
    };
    use crate::status::ReldexStatus;
    use reldex_db_core::{DbError, ErrorKind};
    use reldex_db_driver_api::{NativeError, SqlPosition};

    fn ora_942() -> DbError {
        DbError::new(ErrorKind::Syntax, "table or view does not exist")
            .with_native(NativeError::new(
                942,
                "ORA-00942: table or view does not exist",
            ))
            .with_position(SqlPosition::at_line_column(2, 7))
    }

    #[test]
    fn an_error_crosses_with_its_native_code_and_position() {
        let error = Box::new(ReldexError::from_db_error(&ora_942()));
        let raw = Box::into_raw(error);
        let mut view = ReldexErrorView::default();
        // SAFETY: `raw` is live and `view` is a real, initialized view.
        let status = unsafe { reldex_error_view(raw, std::ptr::from_mut(&mut view)) };
        assert_eq!(status, ReldexStatus::Ok);
        assert_eq!(view.kind, ReldexErrorKind::Syntax as i32);
        assert_eq!(view.session_state, ReldexSessionState::Usable as i32);
        assert!(view.has_native);
        assert_eq!(view.native_code, 942);
        assert!(view.has_line_column);
        assert_eq!((view.line, view.column), (2, 7));
        assert!(!view.has_char_offset);
        // SAFETY: the view's strings borrow from the still-live error.
        let message = unsafe { view.message.as_str() }.expect("utf-8");
        assert!(message.contains("does not exist"), "{message}");
        // SAFETY: `raw` is live and has not been freed.
        unsafe { reldex_error_free(raw) };
    }

    #[test]
    fn the_last_error_is_taken_once() {
        reldex_last_error_clear();
        set_last_error(ora_942());
        let first = reldex_last_error_take();
        assert!(!first.is_null());
        assert!(reldex_last_error_take().is_null(), "an error exists once");
        // SAFETY: `first` came from this library and has not been freed.
        unsafe { reldex_error_free(first) };
        // SAFETY: freeing a null pointer is documented as a no-op.
        unsafe { reldex_error_free(std::ptr::null_mut()) };
    }

    #[test]
    fn a_view_of_a_null_error_is_refused_rather_than_dereferenced() {
        let mut view = ReldexErrorView::default();
        // SAFETY: a null `error` is exactly what this call must refuse.
        let status = unsafe { reldex_error_view(std::ptr::null(), std::ptr::from_mut(&mut view)) };
        assert_eq!(status, ReldexStatus::InvalidArgument);
        reldex_last_error_clear();
    }
}
