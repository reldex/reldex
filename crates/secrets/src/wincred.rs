//! Windows Credential Manager backend (ADR-0007 S4).
//!
//! # The one place outside `crates/ffi` that may use `unsafe`
//!
//! The workspace denies `unsafe_code`. Calling the Credential Manager is four
//! foreign functions (`CredWriteW`, `CredReadW`, `CredDeleteW`, `CredFree`)
//! and there is no safe way to call a foreign function, so this module —
//! only this module, only on Windows — opts out, with the same fence
//! ADR-0003 D2 puts around `crates/ffi`: `unsafe_op_in_unsafe_fn` denied,
//! every `unsafe` block documented (`clippy::undocumented_unsafe_blocks`) and
//! as small as the call it wraps, no business logic in here, and
//! `crates/ffi/tests/fences.rs` listing this *file* (not the crate) as the
//! allowed exception. ADR-0007 S5 records the decision.
//!
//! # What is stored
//!
//! One `CRED_TYPE_GENERIC` credential per profile:
//!
//! | Field | Value |
//! | --- | --- |
//! | target name | `Reldex/profile/<profile UUID>` ([`WindowsCredentialManager::target_name`]) |
//! | blob | the password's UTF-8 bytes, at most [`MAX_SECRET_BYTES`] |
//! | persistence | `CRED_PERSIST_LOCAL_MACHINE` |
//! | comment | [`COMMENT`] |
//! | user name, alias, attributes | none |
//!
//! The target name's `Reldex/` prefix is the namespace: `cmdkey
//! /list:Reldex/*` lists every entry Reldex made, and an uninstaller can
//! enumerate and delete exactly those. The database user name is not stored:
//! it is in the profile already, and a second copy could only disagree.

#![allow(
    unsafe_code,
    reason = "Credential Manager is a foreign API; ADR-0007 S5 fences this module"
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(clippy::missing_safety_doc)]

use std::iter;
use std::ptr;

use reldex_db_driver_api::Secret;
use reldex_workspace::CredentialKey;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_SUCH_LOGON_SESSION, ERROR_NOT_FOUND, FILETIME,
    GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows_sys::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use zeroize::Zeroize;

use crate::store::{CredentialError, CredentialStore, CredentialStoreKind, secret_from_utf8};

/// The largest password the Credential Manager stores, in UTF-8 bytes
/// (`CRED_MAX_CREDENTIAL_BLOB_SIZE`, 5 × 512). A longer one is refused with
/// [`CredentialError::TooLarge`] before anything is written.
pub const MAX_SECRET_BYTES: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize;

/// The comment every Reldex entry carries, shown in Control Panel's
/// Credential Manager next to the target name.
pub const COMMENT: &str = "Reldex connection profile password";

/// The target-name namespace; every entry Reldex writes starts with it.
const NAMESPACE: &str = "Reldex/profile/";

/// The session-wide mutex every Reldex process holds around each Credential
/// Manager call ([`CallLock`]).
const LOCK_NAME: &str = r"Local\com.reldex.reldex.credential-store";

/// How long a call waits for another Reldex call to finish before giving up
/// with [`CredentialError::Backend`] carrying `WAIT_TIMEOUT` (258). A call
/// takes milliseconds; ten seconds means something is stuck.
const LOCK_WAIT_MS: u32 = 10_000;

/// One Credential Manager call at a time, across every thread of every
/// Reldex process in this logon session.
///
/// Measured on Windows 11 (ADR-0007 S4, "Concurrent writers"): calls that
/// write or delete *different* targets at the same time lose updates. Two
/// threads of one process: deleted entries reappear once the process has
/// exited. Two processes: a just-written entry can read back as absent, and
/// deleted ones reappear. Holding a named mutex around each call removes both
/// between Reldex callers. It cannot serialise other applications writing
/// their own credentials at the same moment — that residual race is a
/// platform limitation ADR-0007 records.
///
/// A Win32 mutex is owned by a thread, so the guard must be dropped on the
/// thread that took it; the raw handle makes it `!Send`, which the compiler
/// enforces.
struct CallLock(HANDLE);

impl CallLock {
    fn acquire() -> Result<Self, CredentialError> {
        let name = wide(LOCK_NAME);
        // SAFETY: a null security-attributes pointer asks for the default
        // descriptor; `name` is a NUL-terminated UTF-16 string that outlives
        // the call. Opening an existing mutex of that name is the intent.
        let handle = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(map_error(last_error()));
        }
        // SAFETY: `handle` is the valid mutex handle just returned.
        let outcome = unsafe { WaitForSingleObject(handle, LOCK_WAIT_MS) };
        match outcome {
            // `WAIT_ABANDONED`: a previous holder exited mid-call. The
            // mutex is ours now; the credential set is whatever the system
            // kept, which is all any caller could ever see.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            _ => {
                let code = if outcome == WAIT_TIMEOUT {
                    WAIT_TIMEOUT
                } else {
                    last_error()
                };
                // SAFETY: `handle` is valid, not owned by this thread, and
                // closed exactly once, here.
                unsafe { CloseHandle(handle) };
                Err(CredentialError::Backend {
                    code: i64::from(code),
                })
            }
        }
    }
}

impl Drop for CallLock {
    fn drop(&mut self) {
        // SAFETY: `acquire` returned only after this thread came to own the
        // mutex, and the guard cannot leave this thread (`!Send`), so the
        // release is by its owner; the handle is then closed exactly once.
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

/// Windows Credential Manager: each profile's password is a generic
/// credential of the signed-in user, persisted on this machine and not
/// roamed (ADR-0007 S4 has the persistence choice and why).
///
/// Stateless: every call goes straight to the operating system, so any
/// number of copies may be used from any thread. Calls block briefly (a call
/// into the local security authority) — the workspace service thread's job,
/// never the UI thread's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WindowsCredentialManager;

impl WindowsCredentialManager {
    /// The store.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The target name a profile's password is stored under:
    /// `Reldex/profile/` and the profile's UUID in lowercase hyphenated form.
    ///
    /// Stable: changing it would orphan every saved password.
    #[must_use]
    pub fn target_name(key: &CredentialKey) -> String {
        format!("{NAMESPACE}{key}")
    }

    /// Reads the raw entry: its blob (copied out, the operating system's own
    /// copy wiped before it is freed) and its persistence. `Ok(None)` when
    /// there is none.
    fn read(key: &CredentialKey) -> Result<Option<Entry>, CredentialError> {
        let target = wide(&Self::target_name(key));
        let _one_at_a_time = CallLock::acquire()?;
        let mut raw: *mut CREDENTIALW = ptr::null_mut();
        // SAFETY: `target` is a NUL-terminated UTF-16 string that outlives
        // the call; `raw` is a valid place for the out-pointer; flags must be
        // 0. On success the system owns an allocation we must `CredFree`.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &raw mut raw) };
        if ok == 0 {
            let code = last_error();
            return if code == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(map_error(code))
            };
        }
        let allocation = CredAllocation(raw);

        // SAFETY: `CredReadW` succeeded, so `raw` is non-null and points at a
        // valid, initialised `CREDENTIALW` until `CredFree`, which only
        // `allocation`'s drop calls. The struct is `Copy` (plain pointers and
        // integers); reading it copies no secret.
        let credential = unsafe { raw.read() };
        let size = credential.CredentialBlobSize as usize;
        let mut blob = Vec::with_capacity(size);
        if size > 0 {
            if credential.CredentialBlob.is_null() {
                return Err(CredentialError::Malformed);
            }
            // SAFETY: the blob pointer is non-null and, per the API, valid for
            // `CredentialBlobSize` bytes inside the allocation `CredReadW`
            // returned, which is ours — writable, and not aliased by anything
            // else — until `allocation` is dropped below. The slice does not
            // overlap `credential`, which is a copy on our stack.
            let system_copy =
                unsafe { std::slice::from_raw_parts_mut(credential.CredentialBlob, size) };
            // `with_capacity(size)` then one `extend_from_slice` of `size`
            // bytes: the vector never reallocates, so no stray copy is left.
            blob.extend_from_slice(system_copy);
            system_copy.zeroize();
        }
        drop(allocation);
        Ok(Some(Entry {
            blob,
            persist: credential.Persist,
        }))
    }
}

/// A stored entry, as read. Its blob is wiped when it is dropped, unless
/// [`CredentialStore::get`] has moved it into a [`Secret`] first.
struct Entry {
    blob: Vec<u8>,
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "read by the persistence test only")
    )]
    persist: CRED_PERSIST,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.blob.zeroize();
    }
}

/// Frees what `CredReadW` returned, on every path out of `read`.
struct CredAllocation(*mut CREDENTIALW);

impl Drop for CredAllocation {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `CredReadW` and is freed
        // exactly once, here; nothing uses it afterwards.
        unsafe { CredFree(self.0.cast_const().cast()) };
    }
}

impl CredentialStore for WindowsCredentialManager {
    fn kind(&self) -> CredentialStoreKind {
        CredentialStoreKind::WindowsCredentialManager
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, CredentialError> {
        match Self::read(key)? {
            None => Ok(None),
            Some(mut entry) => secret_from_utf8(std::mem::take(&mut entry.blob)).map(Some),
        }
    }

    fn put(&self, key: &CredentialKey, secret: &Secret) -> Result<(), CredentialError> {
        let bytes = secret.expose().as_bytes();
        if bytes.len() > MAX_SECRET_BYTES {
            return Err(CredentialError::TooLarge {
                max_bytes: MAX_SECRET_BYTES,
            });
        }
        let blob_size = u32::try_from(bytes.len()).map_err(|_| CredentialError::TooLarge {
            max_bytes: MAX_SECRET_BYTES,
        })?;
        let mut target = wide(&Self::target_name(key));
        let mut comment = wide(COMMENT);
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: blob_size,
            // The secret's own bytes, not a copy. `CredWriteW` only reads the
            // blob (its parameter is `const CREDENTIALW*`); the field is
            // `*mut` only because the struct is shared with `CredReadW`.
            CredentialBlob: if bytes.is_empty() {
                ptr::null_mut()
            } else {
                bytes.as_ptr().cast_mut()
            },
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: ptr::null_mut(),
            TargetAlias: ptr::null_mut(),
            UserName: ptr::null_mut(),
        };
        let _one_at_a_time = CallLock::acquire()?;
        // SAFETY: every pointer in `credential` is either null (allowed for
        // the optional fields and for an empty blob) or points into `target`,
        // `comment` or `secret`, all of which outlive the call; the blob is
        // valid for `CredentialBlobSize` bytes and is only read; flags must
        // be 0.
        let ok = unsafe { CredWriteW(&raw const credential, 0) };
        if ok == 0 {
            return Err(map_error(last_error()));
        }
        Ok(())
    }

    fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError> {
        let target = wide(&Self::target_name(key));
        let _one_at_a_time = CallLock::acquire()?;
        // SAFETY: `target` is a NUL-terminated UTF-16 string that outlives
        // the call; flags must be 0.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok == 0 {
            let code = last_error();
            return Err(if code == ERROR_NOT_FOUND {
                CredentialError::NotFound
            } else {
                map_error(code)
            });
        }
        Ok(())
    }
}

/// The calling thread's last Win32 error; read immediately after the failing
/// call, before anything else can overwrite it.
fn last_error() -> WIN32_ERROR {
    // SAFETY: `GetLastError` has no preconditions.
    unsafe { GetLastError() }
}

/// Classifies a Win32 error, keeping the code for anything unclassified.
fn map_error(code: WIN32_ERROR) -> CredentialError {
    match code {
        // "The logon session does not exist or there is no credential set
        // associated with this logon session" — e.g. a network logon.
        ERROR_NO_SUCH_LOGON_SESSION => CredentialError::Unavailable,
        ERROR_ACCESS_DENIED => CredentialError::Denied,
        ERROR_NOT_FOUND => CredentialError::NotFound,
        other => CredentialError::Backend {
            code: i64::from(other),
        },
    }
}

/// A NUL-terminated UTF-16 copy of `text` (never a secret).
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use reldex_workspace::ProfileId;

    use super::*;

    /// Deletes the entry however the test ends.
    struct Cleanup(CredentialKey);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = WindowsCredentialManager.delete(&self.0);
        }
    }

    #[test]
    fn the_target_name_is_namespaced_by_the_profile_uuid() {
        let id = ProfileId::parse("6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f").expect("valid");
        assert_eq!(
            WindowsCredentialManager::target_name(&CredentialKey::for_profile(id)),
            "Reldex/profile/6f1c1f7e-3a4b-4c5d-8e9f-0a1b2c3d4e5f"
        );
    }

    #[test]
    fn win32_errors_are_classified_and_unknown_codes_kept() {
        assert_eq!(
            map_error(ERROR_NO_SUCH_LOGON_SESSION),
            CredentialError::Unavailable
        );
        assert_eq!(map_error(ERROR_ACCESS_DENIED), CredentialError::Denied);
        assert_eq!(map_error(ERROR_NOT_FOUND), CredentialError::NotFound);
        // ERROR_INVALID_PARAMETER.
        assert_eq!(map_error(87), CredentialError::Backend { code: 87 });
    }

    #[test]
    fn the_entry_is_a_generic_credential_persisted_on_this_machine() {
        let key = CredentialKey::for_profile(ProfileId::new_random());
        let _cleanup = Cleanup(key);
        WindowsCredentialManager
            .put(&key, &Secret::new("persistence-check"))
            .expect("put");
        let entry = WindowsCredentialManager::read(&key)
            .expect("read")
            .expect("present");
        assert_eq!(entry.persist, CRED_PERSIST_LOCAL_MACHINE);
        assert_eq!(entry.blob, b"persistence-check");
    }
}
