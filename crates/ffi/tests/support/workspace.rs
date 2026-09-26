//! A workspace on the in-memory store with the memory credential store, for
//! the integration tests that prepare a connect (M3.3) from outside the
//! crate. Nothing here touches the real store, and nothing touches the OS
//! credential store except [`TestWorkspace::open_with_platform_store`], which
//! only the no-store test calls, and only where the platform has no store.

use std::ffi::c_void;
use std::sync::Arc;

use reldex_ffi::{
    ReldexAuthKind, ReldexConnectSummary, ReldexConnectSummaryView, ReldexDatabaseType,
    ReldexEndpointKind, ReldexEnvironmentKind, ReldexPasswordStorageKind, ReldexProfileDetails,
    ReldexSecret, ReldexServiceTargetKind, ReldexSessionRoleKind, ReldexSettingId,
    ReldexSettingLevel, ReldexSettingValue, ReldexStatus, ReldexStr, ReldexTransportKind,
    ReldexValueKind, ReldexWorkspace, ReldexWorkspaceReply, reldex_connect_summary_view,
    reldex_workspace_close, reldex_workspace_create_profile, reldex_workspace_credential_put,
    reldex_workspace_next_reply, reldex_workspace_open, reldex_workspace_prepare_connect,
    reldex_workspace_set_setting, reldex_workspace_set_waker,
};

use super::{WakeSignal, wake_fn};

/// An open workspace and the signal its waker bumps.
pub(crate) struct TestWorkspace {
    handle: *mut ReldexWorkspace,
    signal: Arc<WakeSignal>,
    next_request: std::cell::Cell<u64>,
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        // SAFETY: `handle` is live and this struct holds the only reference;
        // closing unregisters the waker before `signal` is released.
        unsafe { reldex_workspace_close(self.handle) };
    }
}

impl TestWorkspace {
    /// Opens an in-memory store with the memory credential store.
    pub(crate) fn open() -> Self {
        Self::open_with(true)
    }

    /// Opens an in-memory store with the platform's own credential store.
    /// Call it only where the platform has none (every platform but Windows
    /// today): a test must never read or write a real one.
    pub(crate) fn open_with_platform_store() -> Self {
        Self::open_with(false)
    }

    fn open_with(memory_credential_store: bool) -> Self {
        let signal = WakeSignal::for_test();
        let mut handle: *mut ReldexWorkspace = std::ptr::null_mut();
        // SAFETY: `path` is unused (`in_memory: true`); `handle` is a real
        // local out-pointer.
        let status = unsafe {
            reldex_workspace_open(
                ReldexStr::empty(),
                true,
                memory_credential_store,
                1,
                std::ptr::from_mut(&mut handle),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        assert!(!handle.is_null());
        // SAFETY: `handle` is live; `signal` outlives the registration (this
        // struct holds both and closes the workspace first).
        let status = unsafe {
            reldex_workspace_set_waker(
                handle,
                Some(wake_fn()),
                Arc::as_ptr(&signal).cast_mut().cast::<c_void>(),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let workspace = Self {
            handle,
            signal,
            next_request: std::cell::Cell::new(100),
        };
        let opened = workspace.wait_for(1);
        assert!(opened.error.is_null(), "the memory workspace must open");
        workspace
    }

    pub(crate) fn handle(&self) -> *mut ReldexWorkspace {
        self.handle
    }

    /// A request id no earlier call on this workspace used.
    pub(crate) fn request(&self) -> u64 {
        let request = self.next_request.get();
        self.next_request.set(request + 1);
        request
    }

    /// Waits on the waker for `request`'s reply; other replies are dropped
    /// (every caller has one request outstanding at a time).
    pub(crate) fn wait_for(&self, request: u64) -> ReldexWorkspaceReply {
        let mut seen = self.signal.wakes();
        loop {
            // SAFETY: zero is a valid bit pattern for this flat `#[repr(C)]`
            // struct of integers, bools and raw pointers.
            let mut out: ReldexWorkspaceReply = unsafe { std::mem::zeroed() };
            out.struct_size = u32::try_from(size_of::<ReldexWorkspaceReply>()).expect("fits");
            // SAFETY: `handle` is live; `out` is a real local with
            // `struct_size` set.
            let taken =
                unsafe { reldex_workspace_next_reply(self.handle, std::ptr::from_mut(&mut out)) };
            if taken {
                if out.request == request {
                    return out;
                }
                continue;
            }
            let now = self.signal.wait_past(seen);
            assert!(now > seen, "no reply to request {request} arrived in time");
            seen = now;
        }
    }

    /// Creates a profile and returns its id.
    pub(crate) fn create_profile(&self, details: &ReldexProfileDetails) -> [u8; 16] {
        let request = self.request();
        // SAFETY: `handle` is live; `details` is a real local whose strings
        // borrow `'static` or caller-owned text alive for this call.
        let status = unsafe {
            reldex_workspace_create_profile(self.handle, request, std::ptr::from_ref(details))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let created = self.wait_for(request);
        assert!(created.error.is_null(), "the profile must be created");
        created.id
    }

    /// Stores `password` for `id` in the memory credential store.
    pub(crate) fn put_password(&self, id: &[u8; 16], password: &str) {
        let request = self.request();
        // SAFETY: `handle` is live; `id` is 16 readable bytes; `password`
        // borrows text alive for this call.
        let status = unsafe {
            reldex_workspace_credential_put(self.handle, request, id.as_ptr(), str_of(password))
        };
        assert_eq!(status, ReldexStatus::Ok);
        let put = self.wait_for(request);
        assert!(put.error.is_null(), "credential_put failed");
    }

    /// Sets the profile-level connect limit, in seconds.
    pub(crate) fn set_profile_connect_timeout(&self, id: &[u8; 16], seconds: u32) {
        let request = self.request();
        let value = ReldexSettingValue {
            kind: ReldexValueKind::TimeLimit as i32,
            number_value: seconds,
            ..ReldexSettingValue::default()
        };
        // SAFETY: `handle` is live; `id` is 16 readable bytes; `value` is a
        // real local with `struct_size` set.
        let status = unsafe {
            reldex_workspace_set_setting(
                self.handle,
                request,
                ReldexSettingId::ConnectTimeout as i32,
                ReldexSettingLevel::Profile as i32,
                id.as_ptr(),
                std::ptr::from_ref(&value),
            )
        };
        assert_eq!(status, ReldexStatus::Ok);
        let set = self.wait_for(request);
        assert!(set.error.is_null(), "set_setting failed");
    }

    /// Prepares a connect to `id`, with the typed `password` when given, and
    /// returns the reply.
    pub(crate) fn prepare_connect(
        &self,
        id: &[u8; 16],
        password: *const ReldexSecret,
    ) -> ReldexWorkspaceReply {
        let request = self.request();
        // SAFETY: `handle` is live; `id` is 16 readable bytes; `password` is
        // null or a live secret the caller owns.
        let status = unsafe {
            reldex_workspace_prepare_connect(self.handle, request, id.as_ptr(), password)
        };
        assert_eq!(status, ReldexStatus::Ok);
        self.wait_for(request)
    }
}

/// A password-authenticated, host/port/service-name Oracle profile.
///
/// The strings are borrowed, not copied: keep them alive until the profile
/// has been created.
pub(crate) fn profile_details(
    host: &str,
    port: u16,
    service: &str,
    username: &str,
) -> ReldexProfileDetails {
    ReldexProfileDetails {
        struct_size: u32::try_from(size_of::<ReldexProfileDetails>()).expect("fits"),
        name: str_of("M3.3 connect test"),
        database_type: ReldexDatabaseType::Oracle as i32,
        environment: ReldexEnvironmentKind::Test as i32,
        environment_label: ReldexStr::empty(),
        treat_as_production: false,
        endpoint_kind: ReldexEndpointKind::HostPort as i32,
        host: str_of(host),
        port,
        service_target_kind: ReldexServiceTargetKind::ServiceName as i32,
        service_name_or_sid: str_of(service),
        connect_string: ReldexStr::empty(),
        auth_kind: ReldexAuthKind::Password as i32,
        username: str_of(username),
        password_storage: ReldexPasswordStorageKind::CredentialStore as i32,
        role: ReldexSessionRoleKind::Normal as i32,
        transport: ReldexTransportKind::Plain as i32,
        ca_directory: ReldexStr::empty(),
        allow_unenforced_certificate_pin: false,
    }
}

/// Borrows `text` as a [`ReldexStr`] for the duration of one call.
pub(crate) fn str_of(text: &str) -> ReldexStr {
    ReldexStr {
        ptr: text.as_ptr(),
        len: text.len(),
    }
}

/// Reads a connect summary's view.
pub(crate) fn summary_view(summary: *const ReldexConnectSummary) -> ReldexConnectSummaryView {
    let mut view = ReldexConnectSummaryView {
        struct_size: u32::try_from(size_of::<ReldexConnectSummaryView>()).expect("fits"),
        endpoint_kind: 0,
        host: ReldexStr::empty(),
        port: 0,
        service: ReldexStr::empty(),
        connect_string: ReldexStr::empty(),
        tls_mode: 0,
        has_connect_timeout: false,
        connect_timeout_seconds: 0,
        role: 0,
        rewrite_trigger_ddl: false,
        connect_without_limit: false,
        allow_unenforced_certificate_pin: false,
        ca_directory: ReldexStr::empty(),
    };
    // SAFETY: `summary` is live and owned by the caller; `view` is a real
    // local with `struct_size` set.
    let ok = unsafe { reldex_connect_summary_view(summary, std::ptr::from_mut(&mut view)) };
    assert!(ok, "reldex_connect_summary_view refused a live summary");
    view
}
