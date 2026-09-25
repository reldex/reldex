# reldex-secrets

Where a connection profile's password lives between connects: the operating system's secure store,
behind one small trait (`SPEC.md` §17, `ARCHITECTURE.md` §9). Decision record:
[ADR-0007](../../docs/decisions/0007-credential-store.md). Task: M2.10.

**The rule (owner decision 2026-09-20, `phase-1.md` §C.3 item 7):** a password is kept only in the
platform's secure store. Where there is none, the user is asked for it at every connect. There is
no plaintext fallback, and nothing in this crate writes a password anywhere else.

## API

| Item | What it is |
| --- | --- |
| `CredentialStore` | `get(&CredentialKey) -> Result<Option<Secret>, CredentialError>`, `put(&CredentialKey, &Secret)`, `delete(&CredentialKey)`, `kind()`, `describe()`. `Send + Sync`. Every call may block, so call it on the workspace service thread, never the UI thread. |
| `CredentialKey` | The profile's UUID (`reldex_workspace`, ADR-0006 P7), re-exported here. |
| `Secret` | The driver contract's password type (`reldex_db_driver_api`), re-exported here. It has no `Display`, its `Debug` prints `Secret(<redacted>)`, and it wipes itself with `zeroize` on drop. |
| `CredentialStoreKind` | `WindowsCredentialManager`, `Absent`, `Memory` (tests only). `can_store()` says whether a connection dialog should offer "save password". |
| `CredentialError` | `Unavailable`, `NotFound` (from `delete` only; `get` reports absence as `Ok(None)`), `Denied`, `TooLarge { max_bytes }` (per backend), `InvalidSecret` (a control character; nothing written), `Malformed` (an entry Reldex did not write), `Locked { code }` (the store's lock could not be taken), `Backend { code }`. No variant holds a value, so `Debug` and `Display` are safe to log. |
| `WindowsCredentialManager` | Windows only. See below. |
| `NoCredentialStore` | Every call fails with `Unavailable`. Every other platform gets this until its own backend exists. |
| `platform_default()` | Returns `WindowsCredentialManager` on Windows and `NoCredentialStore` elsewhere. |
| `resolve_password(&Profile, &dyn CredentialStore) -> PasswordSource` | The connect flow's rule, written as a function. See below. |
| `MemoryCredentialStore` | `test-support` feature (and `cfg(test)`). An in-process map with `fail_with(error)` and `reads()` hooks, so the core's and the UI's tests can reach every outcome without the real store. It refuses a control character like the real store; it has no size limit. |

### `resolve_password`

```text
profile authentication          store                     -> PasswordSource
------------------------------  ------------------------  -----------------------------------------
External                        (not read)                -> NotNeeded
Password, "prompt each time"    (not read)                -> PromptRequired(PromptEachTime)
Password, "saved in the store"  holds a Reldex entry      -> FromStore(secret)
                                holds nothing             -> PromptRequired(NotStored)
                                Unavailable               -> PromptRequired(StoreUnavailable)
                                any other error           -> PromptRequired(StoreFailed(error))
                                (Malformed, Locked, …)
```

`PasswordSource` has exactly these three variants and is not `#[non_exhaustive]`, so a connect flow
has to handle each one by name. None of them carries a password from anywhere except the store or
the user. A store failure produces a prompt that states the reason, never an error, because
prompting is always a correct way to connect and falling back to anything else never is. The
result feeds `reldex_workspace::connection_params(profile, settings, password, binding)`:
`FromStore(s)` and a typed password become `Some(s)`, and `NotNeeded` becomes `None`.

The rules the connection manager and the connect flow follow with it (ADR-0007 "Consequences"):
store `put` first and flip the profile's flag only after it succeeds; when clearing, flip the flag
even if the delete fails, and report the leftover entry; if the database refuses a `FromStore`
password, prompt next time and offer to update it, and **never retry a stored password
automatically**, since each attempt counts towards the account's lockout limit.

## Windows Credential Manager

| Field | Value |
| --- | --- |
| type | `CRED_TYPE_GENERIC` |
| target name | `Reldex/profile/<profile UUID>` (lowercase, hyphenated) |
| blob | `RLDX`, version byte `0x01`, then the password's UTF-8 bytes (no control character) |
| limit | 2,555 password bytes (`CRED_MAX_CREDENTIAL_BLOB_SIZE` 2,560 less the 5-byte prefix): `WindowsCredentialManager::MAX_SECRET_BYTES`; longer is refused with `TooLarge` before a write |
| persistence | `CRED_PERSIST_LOCAL_MACHINE`: this user, this machine, kept across logons, not roamed |
| comment | `Reldex credential v1` (`WindowsCredentialManager::COMMENT`) |
| user name, alias, attributes | none (the database user name stays in the profile) |

- **Why local-machine persistence.** `SESSION` loses the password at logoff, so "save password"
  would stop working after every reboot. `ENTERPRISE` roams the password to every machine the
  user's domain profile reaches, which is wider than the user asked for, and the profile store it
  belongs to is itself local (`%LOCALAPPDATA%`, ADR-0006 P5). ADR-0007 S4.
- **Only Reldex entries are passwords.** `get` decodes only the version-1 entry above. Anything
  else under a Reldex target name is `Malformed`, and the connect flow prompts instead. That covers
  a missing prefix, another version, or a payload with a control character, such as the UTF-16 that
  `cmdkey /generic:… /pass:…` writes. A wrong password is never sent to the database automatically,
  where it would count towards the account's failed-login limit. ADR-0007 S8.
- **Namespace and cleanup.** Every entry's target starts with `Reldex/`. `cmdkey /list:Reldex/*`
  lists exactly Reldex's entries, and an uninstaller can delete exactly those (for each target:
  `cmdkey /delete:<target>`). `cmdkey` never prints a password.
- **Concurrency.** Measured on Windows 11 with local-machine persistence: when calls write or
  delete *different* targets at the same time, the Credential Manager loses updates.
  - A read straight after a delete is clean, yet the deleted entry reappears later, while the
    process is still running.
  - Across processes, an entry that was just written can also read back as absent.
  - Session persistence does not lose updates, but it forgets passwords at logoff.

  The on-disk credential file is rewritten and reloaded as a whole. Every call therefore holds a
  per-user named mutex, `Local\Reldex.CredentialStore.<user SID>`:
  - It is created with a descriptor granting the user only `SYNCHRONIZE | MUTEX_MODIFY_STATE`, at
    medium integrity, and opened with exactly those rights.
  - It waits at most 2 s.
  - Failing to take it is `Locked { code }`: `258` means it was held too long, `5` means an object
    of that name refuses access. It is never reported as `Denied`.

  The mutex cannot serialise *other* applications that write their own credentials at the same
  moment. That residual race is a recorded platform limitation, and the planned orphan sweep
  (M2.14) cleans up after it. So is the copy another tool's entry (such as `cmdkey`'s) can leave
  behind when Reldex saves over it and later deletes it; that copy reads as `Malformed`.
  `tests/windows_credential_manager.rs`'s own unlocked `cmdkey` writer (the "written by another
  tool" test) self-inflicted exactly this race once on CI by running concurrently with the
  regression test (ADR-0007 S4 "CI incident, diagnosed"); the suite now isolates that one test from
  the rest so it can never happen again from within this binary.
- **Hygiene.** `put` builds the entry in one buffer of exactly the right size, wiped when dropped.
  `get` copies the blob into a buffer of exactly the right size, wipes the system's copy with
  `zeroize` before calling `CredFree`, and wipes bytes that fail decoding. The wipe reduces how many
  copies exist. It does not guarantee that none survive.
- **`unsafe`.** `src/wincred.rs` is the only file in this crate, and the only product code outside
  `crates/ffi`, that opts out of the workspace's `unsafe_code = "deny"`. It uses the same fence as
  ADR-0003 D2: `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks` are denied, every
  block has a `SAFETY:` comment, and `crates/ffi/tests/fences.rs` allows this one *file*, not the
  crate. ADR-0007 S5.

## Other platforms

Apple Keychain (macOS, iOS), Android Keystore and Linux Secret Service (`SPEC.md` §17) are later
tasks. Until each one lands, `platform_default()` returns `NoCredentialStore` on that platform and
the password is asked for at every connect. The crate compiles on every target: the Windows backend
and its `windows-sys` dependency are `cfg(windows)`.

## Dependencies

| Crate | Version | Licence | New to the graph? | Why |
| --- | --- | --- | --- | --- |
| `reldex-workspace` | — | GPL-3.0-or-later | — | `CredentialKey`, `Profile` |
| `reldex-db-driver-api` | — | GPL-3.0-or-later | — | `Secret` |
| `zeroize` (`alloc`) | 1.9.0 | Apache-2.0 OR MIT | no (via `aws-lc-rs`/`der`) | wipes transient buffers; also used by `Secret` now (ADR-0007 S6) |
| `windows-sys` (Windows only; `Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`, `Win32_Security_Credentials`, `Win32_System_Threading`) | 0.61.2 | MIT OR Apache-2.0 | **yes** | the Credential Manager calls, the user's SID, the lock's security descriptor and the named mutex |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | no (via `chrono`) | `windows-sys`'s only dependency |

We use `windows-sys` rather than the `windows` crate that the owner approved (same Microsoft project,
same licence) because this crate needs only raw function declarations. `windows` adds COM/WinRT
wrapper layers on top of those. We do not use `keyring` because on Windows it wraps the same calls
while imposing its own target-name scheme and error type, and on Linux it brings a D-Bus stack.
Dev-dependency only: `serde_json` (the dependency-rule test).

## Tests

`cargo test -p reldex-secrets` needs no database:

- **Unit tests.**
  - The trait contract against the memory store and, on Windows, the real Credential Manager: Thai,
    empty and 512-byte passwords; overwrite; isolation; delete; control characters refused.
  - The entry format: v1 round trip; `cmdkey`'s UTF-16, other magic, a future version and control
    bytes all `Malformed`.
  - The `resolve_password` truth table (3 authentication modes × 12 store states), including proof
    that the store is not read for "prompt each time" or external authentication.
  - Exact `Debug`/`Display` of every error, and `Debug` of a resolved password and of the memory
    store.
  - On Windows: the stored entry is `RLDX\x01…` with `CRED_PERSIST_LOCAL_MACHINE` and the v1
    comment; raw non-v1 entries read as `Malformed`; the lock name carries the SID; a squatted lock
    gives `Locked { code: 5 }` and a held one `Locked { code: 258 }`.
- **`tests/windows_credential_manager.rs`** (Windows only). Real round trips under random profile
  UUIDs, with `cmdkey` as an independent witness to the target name. Thai and emoji, empty, 512
  bytes, and 2,555 bytes are accepted; 2,556 bytes, and a control character, are refused and
  nothing is written. Entries `cmdkey` wrote ("Hunter2x" and "กข") are `Malformed` and lead to a
  prompt. `resolve_password` runs against the real store. Two processes of two threads each lose no update.
  That test fails 5 runs out of 5 when the mutex is removed. Every test removes what it wrote, even
  when it fails.
- **`tests/lock_squat.rs`** (Windows only). A PowerShell process squats the real lock with an
  empty DACL. Every call fails at once with `Locked { code: 5 }`, and the connect flow prompts. Once
  the squatter lets go, the store works again.
- **`tests/dependency_rules.rs`.** This crate's dependencies are exactly those listed above. Nothing
  below it depends on it. The driver contract's only production dependency is `zeroize`.
