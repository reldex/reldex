//! Windows Credential Manager backend (ADR-0007 S4, S8).
//!
//! # The one place outside `crates/ffi` that may use `unsafe`
//!
//! The workspace denies `unsafe_code`. Calling the Credential Manager and
//! taking the lock around it are foreign functions, and there is no safe way
//! to call a foreign function, so this module — only this module, only on
//! Windows — opts out, with the same fence ADR-0003 D2 puts around
//! `crates/ffi`: `unsafe_op_in_unsafe_fn` denied, every `unsafe` block
//! documented (`clippy::undocumented_unsafe_blocks`) and wrapping one call,
//! no business logic in here (the entry format is the safe `format`
//! module), and `crates/ffi/tests/fences.rs` listing this *file* (not the
//! crate) as the allowed exception. ADR-0007 S5 records the decision.
//!
//! # What is stored
//!
//! One `CRED_TYPE_GENERIC` credential per profile:
//!
//! | Field | Value |
//! | --- | --- |
//! | target name | `Reldex/profile/<profile UUID>` ([`WindowsCredentialManager::target_name`]) |
//! | blob | `RLDX`, version byte `0x01`, then the password's UTF-8 bytes — at most [`WindowsCredentialManager::MAX_SECRET_BYTES`] of them |
//! | persistence | `CRED_PERSIST_LOCAL_MACHINE` |
//! | comment | [`WindowsCredentialManager::COMMENT`] |
//! | user name, alias, attributes | none |
//!
//! The target name's `Reldex/` prefix is the namespace: `cmdkey
//! /list:Reldex/*` lists every entry Reldex made, and an uninstaller can
//! enumerate and delete exactly those. The versioned blob prefix is how an
//! entry is recognised as Reldex's own: anything else under that name —
//! `cmdkey /generic:… /pass:…` stores UTF-16 — is `Malformed` and never
//! reaches a database (ADR-0007 S8). The database user name is not stored:
//! it is in the profile already, and a second copy could only disagree.

#![allow(
    unsafe_code,
    reason = "Credential Manager is a foreign API; ADR-0007 S5 fences this module"
)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::iter;
use std::ptr;
use std::sync::OnceLock;

use reldex_db_driver_api::Secret;
use reldex_workspace::CredentialKey;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_SUCH_LOGON_SESSION, ERROR_NOT_FOUND, FILETIME,
    GetLastError, HANDLE, LocalFree, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    CREDENTIALW, CredDeleteW, CredFree, CredReadW, CredWriteW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexExW, GetCurrentProcess, MUTEX_MODIFY_STATE, OpenProcessToken, ReleaseMutex,
    SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows_sys::core::PWSTR;
use zeroize::Zeroize;

use crate::format;
use crate::store::{CredentialError, CredentialStore, CredentialStoreKind};

/// The target-name namespace; every entry Reldex writes starts with it.
const NAMESPACE: &str = "Reldex/profile/";

/// The lock's name, followed by the user's SID: one lock per user in this
/// logon session's namespace, so two users on one machine never contend and
/// nobody else's object of that name can be the lock by accident.
const LOCK_PREFIX: &str = r"Local\Reldex.CredentialStore.";

/// How long a call waits for another Reldex call to finish before failing
/// with [`CredentialError::Locked`] carrying `WAIT_TIMEOUT` (258).
/// Uncontended, a read takes about 0.3 ms and a write about 14 ms; two
/// seconds means something is stuck, and the connect flow prompts instead.
const LOCK_WAIT_MS: u32 = 2_000;

/// The only rights a caller needs on the lock: wait on it, release it. Asked
/// for explicitly (not all-access) and granted to the user's SID by the
/// lock's own security descriptor, so an elevated and a normal process of
/// the same user can both open it whichever created it.
const LOCK_ACCESS: u32 = SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE;

/// The current user's SID in string form, read once.
static USER_SID: OnceLock<String> = OnceLock::new();

/// Windows Credential Manager: each profile's password is a generic
/// credential of the signed-in user, persisted on this machine and not
/// roamed (ADR-0007 S4 has the persistence choice and why).
///
/// Stateless: every call goes to the operating system, under a lock shared by
/// every Reldex process of this user, so any number of copies may be used
/// from any thread. Calls block briefly — the workspace service thread's job,
/// never the UI thread's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WindowsCredentialManager;

impl WindowsCredentialManager {
    /// The largest password this backend stores, in UTF-8 bytes: the
    /// Credential Manager's blob limit (`CRED_MAX_CREDENTIAL_BLOB_SIZE`,
    /// 2,560) less the 5-byte Reldex prefix — **2,555**. A longer one is
    /// refused with [`CredentialError::TooLarge`] before anything is written.
    /// Other backends have their own limits.
    pub const MAX_SECRET_BYTES: usize = CRED_MAX_CREDENTIAL_BLOB_SIZE as usize - format::PREFIX_LEN;

    /// The comment every Reldex entry carries, shown in Control Panel's
    /// Credential Manager next to the target name. It names the format
    /// version for a human; the blob prefix is what the code checks.
    pub const COMMENT: &'static str = "Reldex credential v1";

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
    /// copy wiped before it is freed), persistence and comment. `Ok(None)`
    /// when there is none.
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
        let comment = if credential.Comment.is_null() {
            String::new()
        } else {
            // SAFETY: a non-null `Comment` is a NUL-terminated UTF-16 string
            // inside the allocation, alive until `allocation` is dropped.
            unsafe { read_wide(credential.Comment) }
        };
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
            comment,
        }))
    }

    /// Writes `blob` as the entry for `key`, exactly as given. `put` passes
    /// a version-1 Reldex entry; tests pass others.
    fn write(key: &CredentialKey, blob: &[u8]) -> Result<(), CredentialError> {
        let blob_size = u32::try_from(blob.len()).map_err(|_| CredentialError::TooLarge {
            max_bytes: Self::MAX_SECRET_BYTES,
        })?;
        let mut target = wide(&Self::target_name(key));
        let mut comment = wide(Self::COMMENT);
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
            // `CredWriteW` only reads the blob (its parameter is
            // `const CREDENTIALW*`); the field is `*mut` only because the
            // struct is shared with `CredReadW`.
            CredentialBlob: if blob.is_empty() {
                ptr::null_mut()
            } else {
                blob.as_ptr().cast_mut()
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
        // `comment` or `blob`, all of which outlive the call; the blob is
        // valid for `CredentialBlobSize` bytes and is only read; flags must
        // be 0.
        let ok = unsafe { CredWriteW(&raw const credential, 0) };
        if ok == 0 {
            return Err(map_error(last_error()));
        }
        Ok(())
    }
}

/// A stored entry, as read. Its blob is wiped when it is dropped, unless
/// [`CredentialStore::get`] has moved it into a [`Secret`] first.
struct Entry {
    blob: Vec<u8>,
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the format tests only"))]
    persist: CRED_PERSIST,
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the format tests only"))]
    comment: String,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.blob.zeroize();
    }
}

impl CredentialStore for WindowsCredentialManager {
    fn kind(&self) -> CredentialStoreKind {
        CredentialStoreKind::WindowsCredentialManager
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, CredentialError> {
        match Self::read(key)? {
            None => Ok(None),
            Some(mut entry) => format::decode(std::mem::take(&mut entry.blob)).map(Some),
        }
    }

    fn put(&self, key: &CredentialKey, secret: &Secret) -> Result<(), CredentialError> {
        if secret.expose().len() > Self::MAX_SECRET_BYTES {
            return Err(CredentialError::TooLarge {
                max_bytes: Self::MAX_SECRET_BYTES,
            });
        }
        // Prefix + password in one exact-size buffer, wiped when dropped.
        let blob = format::encode(secret)?;
        Self::write(key, &blob)
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

/// One Credential Manager call at a time, across every thread of every
/// Reldex process of this user in this logon session.
///
/// Measured on Windows 11 (ADR-0007 S4, "Concurrent writers"): with
/// `CRED_PERSIST_LOCAL_MACHINE`, calls that write or delete *different*
/// targets at the same time lose updates — the on-disk credential file is
/// rewritten and reloaded as a whole. A read straight after a delete is
/// clean, yet the deleted entry reappears later, while the process is still
/// running; across two processes a just-written entry can also read back as
/// absent. (`CRED_PERSIST_SESSION`, which never touches the file, lost
/// nothing — but forgets saved passwords at logoff.) A process-wide mutex
/// was not enough across processes; this named one removes the loss between
/// Reldex callers. It cannot serialise other applications writing their own
/// credentials at the same moment — a residual race ADR-0007 records, with
/// the orphan sweep (M2.14) as its remedy.
///
/// A Win32 mutex is owned by a thread, so the guard must be dropped on the
/// thread that took it; the raw handle makes it `!Send`, which the compiler
/// enforces.
struct CallLock {
    mutex: OwnedHandle,
}

impl CallLock {
    /// Takes the user's lock, creating it on first use.
    fn acquire() -> Result<Self, CredentialError> {
        let sid = user_sid()?;
        Self::acquire_named(&format!("{LOCK_PREFIX}{sid}"), &lock_descriptor(sid))
    }

    /// Takes the lock called `name`, creating it with the security
    /// descriptor `sddl` if it does not exist yet. Every failure is
    /// [`CredentialError::Locked`] with the Win32 code: `5` when an object of
    /// that name refuses the two rights asked for, `258` when another holder
    /// keeps it past [`LOCK_WAIT_MS`].
    fn acquire_named(name: &str, sddl: &str) -> Result<Self, CredentialError> {
        let mutex = create_mutex(name, sddl, 0).map_err(locked)?;
        // SAFETY: `mutex` holds a valid handle with `SYNCHRONIZE` access.
        let outcome = unsafe { WaitForSingleObject(mutex.0, LOCK_WAIT_MS) };
        match outcome {
            // `WAIT_ABANDONED`: a previous holder exited mid-call. The
            // mutex is ours now; the credential set is whatever the system
            // kept, which is all any caller could ever see.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { mutex }),
            WAIT_TIMEOUT => Err(locked(WAIT_TIMEOUT)),
            _ => Err(locked(last_error())),
        }
    }
}

impl Drop for CallLock {
    fn drop(&mut self) {
        // SAFETY: `acquire_named` returned only after this thread came to own
        // the mutex, and the guard cannot leave this thread (`!Send`), so the
        // release is by its owner. The handle is closed afterwards by
        // `OwnedHandle`'s own drop.
        unsafe { ReleaseMutex(self.mutex.0) };
    }
}

/// The lock's security descriptor: the user's SID may wait on it and release
/// it and nothing more, and its integrity label is medium with no write-up,
/// so a normal process can use a lock an elevated one created.
fn lock_descriptor(sid: &str) -> String {
    format!("D:(A;;0x{LOCK_ACCESS:X};;;{sid})S:(ML;;NW;;;ME)")
}

/// Opens the mutex `name` with [`LOCK_ACCESS`], or creates it with the
/// security descriptor `sddl` (and, with `CREATE_MUTEX_INITIAL_OWNER` in
/// `flags`, owned by this thread).
fn create_mutex(name: &str, sddl: &str, flags: u32) -> Result<OwnedHandle, WIN32_ERROR> {
    let name = wide(name);
    let sddl = wide(sddl);
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `sddl` is a NUL-terminated UTF-16 string that outlives the
    // call; `descriptor` is a valid out-pointer; the size out-pointer may be
    // null. On success the descriptor is ours to `LocalFree`.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    let descriptor = LocalMemory(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: `attributes` and the descriptor it points to, and `name`, all
    // outlive the call. An existing mutex of that name is opened with exactly
    // `LOCK_ACCESS` (the descriptor is then ignored); a new one is created
    // with it.
    let handle =
        unsafe { CreateMutexExW(&raw const attributes, name.as_ptr(), flags, LOCK_ACCESS) };
    if handle.is_null() {
        return Err(last_error());
    }
    Ok(OwnedHandle(handle))
}

/// The current user's SID, e.g. `S-1-5-21-…-1001`, read from the process
/// token once and remembered.
fn user_sid() -> Result<&'static str, CredentialError> {
    if let Some(sid) = USER_SID.get() {
        return Ok(sid);
    }
    let sid = query_user_sid().map_err(locked)?;
    Ok(USER_SID.get_or_init(|| sid))
}

fn query_user_sid() -> Result<String, WIN32_ERROR> {
    // SAFETY: `GetCurrentProcess` has no preconditions; its pseudo-handle
    // needs no closing.
    let process = unsafe { GetCurrentProcess() };
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is the current-process pseudo-handle and `token` a
    // valid out-pointer.
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return Err(last_error());
    }
    let token = OwnedHandle(token);

    let mut needed: u32 = 0;
    // SAFETY: a null buffer of length 0 asks only for the size, written to
    // the valid out-pointer `needed`. The call "fails" with
    // `ERROR_INSUFFICIENT_BUFFER` by design.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(last_error());
    }
    // `u64` words: 8-byte aligned, as the `TOKEN_USER` at its start needs.
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` is valid and writable for at least `needed` bytes and
    // aligned for `TOKEN_USER`; `needed` is a valid out-pointer.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: the call succeeded, so the buffer starts with an initialised
    // `TOKEN_USER`; it is `Copy` (a pointer into `buffer` and flags).
    let user = unsafe { buffer.as_ptr().cast::<TOKEN_USER>().read() };
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: `user.User.Sid` points into `buffer`, still alive; `text` is a
    // valid out-pointer. On success the string is ours to `LocalFree`.
    let ok = unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) };
    if ok == 0 {
        return Err(last_error());
    }
    let text = LocalMemory(text.cast());
    // SAFETY: `ConvertSidToStringSidW` returned a NUL-terminated UTF-16
    // string, alive until `text` is dropped.
    Ok(unsafe { read_wide(text.0.cast()) })
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

/// A kernel handle closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid (every constructor checks) and closed
        // exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Memory the system allocated with `LocalAlloc`, freed on drop.
struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        // SAFETY: the pointer came from an API documented to allocate with
        // `LocalAlloc` and is freed exactly once, here.
        unsafe { LocalFree(self.0) };
    }
}

/// Reads a NUL-terminated UTF-16 string (never a secret: a SID or a
/// comment).
///
/// # Safety
///
/// `text` must point at a NUL-terminated UTF-16 string that stays alive for
/// the call.
unsafe fn read_wide(text: *const u16) -> String {
    let mut len = 0;
    // SAFETY: the caller guarantees a terminating NUL, so every index up to
    // and including it is in bounds.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the NUL are initialised and in bounds.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    String::from_utf16_lossy(units)
}

/// The calling thread's last Win32 error; read immediately after the failing
/// call, before anything else can overwrite it.
fn last_error() -> WIN32_ERROR {
    // SAFETY: `GetLastError` has no preconditions.
    unsafe { GetLastError() }
}

/// A lock failure, with its Win32 code.
fn locked(code: WIN32_ERROR) -> CredentialError {
    CredentialError::Locked {
        code: i64::from(code),
    }
}

/// Classifies a Win32 error from a credential call, keeping the code for
/// anything unclassified.
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use reldex_workspace::ProfileId;
    use windows_sys::Win32::System::Threading::CREATE_MUTEX_INITIAL_OWNER;

    use super::*;

    /// Deletes the entry however the test ends.
    struct Cleanup(CredentialKey);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = WindowsCredentialManager.delete(&self.0);
        }
    }

    fn fresh_key() -> CredentialKey {
        CredentialKey::for_profile(ProfileId::new_random())
    }

    /// A lock name no other test and no real Reldex process uses.
    fn private_lock_name() -> String {
        format!(
            r"Local\Reldex.CredentialStore.test-{}",
            ProfileId::new_random()
        )
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
    fn the_entry_is_a_v1_generic_credential_persisted_on_this_machine() {
        let key = fresh_key();
        let _cleanup = Cleanup(key);
        WindowsCredentialManager
            .put(&key, &Secret::new("persistence-check"))
            .expect("put");
        let entry = WindowsCredentialManager::read(&key)
            .expect("read")
            .expect("present");
        assert_eq!(entry.persist, CRED_PERSIST_LOCAL_MACHINE);
        assert_eq!(entry.blob, b"RLDX\x01persistence-check");
        assert_eq!(entry.comment, "Reldex credential v1");
        assert_eq!(WindowsCredentialManager::MAX_SECRET_BYTES, 2555);
    }

    #[test]
    fn an_entry_without_the_v1_prefix_is_malformed() {
        let key = fresh_key();
        let _cleanup = Cleanup(key);
        for blob in [
            &b"RLDX\x02future-format"[..],
            b"no prefix at all",
            b"",
            b"RLDX\x01nul\x00inside",
        ] {
            WindowsCredentialManager::write(&key, blob).expect("raw write");
            assert_eq!(
                WindowsCredentialManager
                    .get(&key)
                    .map(|found| found.is_some()),
                Err(CredentialError::Malformed),
                "{blob:?}"
            );
        }
        WindowsCredentialManager::write(&key, b"RLDX\x01ok").expect("raw write");
        assert_eq!(
            WindowsCredentialManager
                .get(&key)
                .expect("get")
                .expect("present")
                .expose(),
            "ok"
        );
    }

    #[test]
    fn the_lock_name_carries_the_users_sid() {
        let sid = user_sid().expect("sid");
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert_eq!(
            lock_descriptor(sid),
            format!("D:(A;;0x100001;;;{sid})S:(ML;;NW;;;ME)")
        );
    }

    #[test]
    fn a_squatted_lock_is_reported_as_locked_not_denied() {
        // Another program created an object of the lock's name with an empty
        // DACL: nobody may open it, not even this user.
        let name = private_lock_name();
        let _squatter = create_mutex(&name, "D:", 0).expect("squatter");
        let sid = user_sid().expect("sid");
        assert_eq!(
            CallLock::acquire_named(&name, &lock_descriptor(sid)).map(drop),
            Err(CredentialError::Locked { code: 5 })
        );
    }

    #[test]
    fn a_lock_held_elsewhere_times_out_as_locked() {
        let name = private_lock_name();
        let sid = user_sid().expect("sid");
        let descriptor = lock_descriptor(sid);
        let (held, release) = (mpsc::channel(), mpsc::channel::<()>());
        let holder = {
            let (name, descriptor) = (name.clone(), descriptor.clone());
            let (tell, wait) = (held.0, release.1);
            std::thread::spawn(move || {
                let mutex = create_mutex(&name, &descriptor, CREATE_MUTEX_INITIAL_OWNER)
                    .expect("holder creates and owns it");
                tell.send(()).expect("tell");
                wait.recv().expect("wait");
                let _lock = CallLock { mutex };
            })
        };
        held.1.recv().expect("held");
        let started = Instant::now();
        assert_eq!(
            CallLock::acquire_named(&name, &descriptor).map(drop),
            Err(CredentialError::Locked { code: 258 })
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(1_900), "{waited:?}");
        release.0.send(()).expect("release");
        holder.join().expect("holder");
        // Released: the next caller gets it at once.
        let lock = CallLock::acquire_named(&name, &descriptor).expect("free again");
        drop(lock);
    }
}
