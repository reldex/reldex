# 0007 — Credential storage: `CredentialStore`, the Windows Credential Manager, no fallback

**Status:** Accepted. It implements the owner decision of 2026-09-20 (`docs/exec-plans/active/phase-1.md`
§C.3 item 7): Windows Credential Manager for Phase 1, and when no store is available the user is
prompted every time, with no plaintext fallback ever. It also settles the two points earlier ADRs
explicitly deferred to "the credential-storage ADR": whether `Secret` uses `zeroize` (ADR-0002, accepted
item 4) and what spans the platform stores (`ARCHITECTURE.md` §13 item 9). The owner may want to
look at two points specifically: S5, a second, file-sized exception to the workspace's `unsafe`
ban, and S6, the driver contract's first production dependency.
**Date:** 2026-09-25
**Task:** M2.10 ★ (`phase-1.md` §C.2)

## Context

`SPEC.md` §17 requires passwords in "platform secure storage where available: Windows Credential
Manager, Apple Keychain, Android Keystore, Linux Secret Service". `ARCHITECTURE.md` §9 asks for "a
single core abstraction", and §13 item 9 asks what that abstraction is and what the fallback is when
no store exists. The owner already answered the second question: there is none, and the user is
prompted each time. ADR-0006 P7 built the seam. `reldex_workspace::CredentialKey` is the profile's
random UUID. A profile records only the `PasswordStorage` flag (`CredentialStore` or
`PromptEachTime`), and no column can hold a password. The password travels from the credential
store into `connection_params` as a `Secret`.

The M2.10 acceptance criteria are: the password round-trips through Credential Manager keyed by the
profile UUID; the absence of a store means prompt each time, never a plaintext fallback; nothing
secret appears in logs or `Debug`; and the licence of every new dependency is recorded. The binding
constraints from `AGENTS.md` are vendor-neutral core layers, explicit typed APIs, no I/O on the UI
thread, no credentials in logs, and no production dependency without a documented reason.

## Decision

### S1 — A crate of its own: `crates/secrets` (`reldex-secrets`)

It depends on `reldex-workspace` (`CredentialKey`, `Profile`), `reldex-db-driver-api` (`Secret`),
`zeroize`, and, on Windows only, `windows-sys`. Nothing below it depends on it.

- **The key and the secret live where they already are.** `CredentialKey` is in `reldex-workspace`
  (ADR-0006 P7) and `Secret` is in the driver contract, where `ConnectionParams` carries it. The
  crate uses both and moves neither.
- **`reldex-workspace` does not depend on this crate.** The SQLite store, which must never hold a
  secret, cannot reach code that holds one. The composition root (`crates/ffi`, M2.11) owns the
  `Store` and the `CredentialStore` side by side, just as it does the `Store` and the
  `SessionRegistry`.
- `tests/dependency_rules.rs` checks the crate's own normal dependencies (exact list, with
  `windows-sys` limited to `cfg(windows)`). It also checks that workspace, db-core, the contract,
  sql-text and every driver depend on neither this crate nor `windows-sys`.

### S2 — The trait

```rust
pub trait CredentialStore: Send + Sync {
    fn kind(&self) -> CredentialStoreKind;          // WindowsCredentialManager | Absent | Memory
    fn describe(&self) -> &'static str;             // diagnostics, English; UI text comes from kind()
    fn get(&self, key: &CredentialKey) -> Result<Option<Secret>, CredentialError>;
    fn put(&self, key: &CredentialKey, secret: &Secret) -> Result<(), CredentialError>;
    fn delete(&self, key: &CredentialKey) -> Result<(), CredentialError>;
}
```

- **Absence.** `get` reports absence as `Ok(None)`. `delete` of an absent key is
  `Err(NotFound)`, so a caller that only wants it gone treats that as success. `put` replaces.
- **`&self` and `Send + Sync`.** The platform stores serialise their own calls, and one instance is
  shared (`Arc<dyn CredentialStore>`).
- **Blocking.** Every call can block. Windows calls go to the local security authority, and a
  future Keychain call can show a system prompt. The calls belong on the workspace service thread,
  never the UI thread (`ARCHITECTURE.md` §6) and never a session worker.
- **`CredentialError` is value-free.** Its variants are `Unavailable` (no store, or unusable from
  this logon session), `NotFound`, `Denied`, `TooLarge { max_bytes }` (nothing written),
  `Malformed` (an entry whose bytes are not UTF-8, i.e. not one Reldex wrote), and
  `Backend { code: i64 }` (the platform's code preserved: a Win32 `u32` or an Apple `OSStatus`
  `i32` both fit). No variant carries a key, a target name or a secret, and exact `Debug`/`Display`
  renderings are tested. `#[non_exhaustive]`.
- `CredentialStoreKind::can_store()` tells a connection dialog whether to offer "save password" at
  all.

### S3 — Absence of a store, and the connect-flow rule as a function

`NoCredentialStore` fails every call with `Unavailable`, and `platform_default()` returns it on
every platform except Windows. It exists so that "no store" is a first-class, tested case rather
than an `Option` a caller might forget to check.

`resolve_password(&Profile, &dyn CredentialStore) -> PasswordSource` encodes the rule in one place:

| Profile | Store | `PasswordSource` |
| --- | --- | --- |
| external authentication | not read | `NotNeeded` |
| password, prompt each time | not read, even if it still holds an old entry | `PromptRequired(PromptEachTime)` |
| password, saved in the store | holds it | `FromStore(secret)` |
| | holds nothing (or a store answers `NotFound`) | `PromptRequired(NotStored)` |
| | `Unavailable` | `PromptRequired(StoreUnavailable)` |
| | any other error | `PromptRequired(StoreFailed(error))` |

`PasswordSource` has exactly three variants and is deliberately **not** `#[non_exhaustive]`, so a
connect flow must name each one. No variant carries a password from anywhere except the store or
the user. The function returns `PasswordSource`, not `Result`. A store failure becomes a prompt
with its reason: prompting is always a correct way to connect and falling back to anything else
never is, so no error path exists for a caller to handle differently. Unknown future
`Authentication` or `PasswordStorage` variants (both `#[non_exhaustive]` upstream) never read the
store. A future mechanism that does need a password still hits `connection_params`'s
`PasswordRequired`, so it can be filled only from the store or the user.

### S4 — The Windows backend

| Field | Value | Why |
| --- | --- | --- |
| API | `CredWriteW`/`CredReadW`/`CredDeleteW`/`CredFree` | Credential Manager, per the owner decision |
| type | `CRED_TYPE_GENERIC` | not a Windows logon credential; no domain semantics |
| target name | `Reldex/profile/<uuid>`, lowercase hyphenated | `Reldex/` namespaces every entry, so `cmdkey /list:Reldex/*` lists exactly Reldex's entries and an uninstaller can delete exactly those; stable forever, since changing it orphans every saved password |
| blob | the password's UTF-8 bytes, ≤ 2,560 (`CRED_MAX_CREDENTIAL_BLOB_SIZE`) | `Secret` is UTF-8; a longer password is refused with `TooLarge` before any write (tested at 2,560 and 2,561) |
| persistence | `CRED_PERSIST_LOCAL_MACHINE` | see below |
| comment | `Reldex connection profile password` | shown in Control Panel next to the target |
| user name / alias / attributes | none | the database user name is in the profile; a second copy could only disagree |

**Persistence scope.** `CRED_PERSIST_SESSION` drops the entry at logoff, so "save password" would
stop working after every reboot. That is the promise the user made a choice about, broken silently.
`CRED_PERSIST_ENTERPRISE` roams the entry with the user's domain profile to every machine it
reaches, which is wider exposure than "save password on this computer" asks for. It would also
disagree with the profile store the entry belongs to, which is deliberately local
(`%LOCALAPPDATA%`, not roaming, ADR-0006 P5). A profile copied to another machine prompts there,
and that is correct. `CRED_PERSIST_LOCAL_MACHINE` is this user on this machine, across logons, and
is not roamed.

**Concurrent writers (measured, fixed within Reldex).** While testing on Windows 11
(10.0.26200), the Credential Manager lost updates when calls wrote or deleted *different* targets
at the same moment:

- two threads in one process: every in-process read looked right, but deleted entries were present
  again once the process had exited (14 of 600 put/get/delete cycles);
- two processes: additionally, an entry that had just been written read back as absent in the
  writer, and its delete then failed with `NotFound` (18 entries left behind and 5 lost writes in
  600 cycles).

Every call now holds a named mutex, `Local\com.reldex.reldex.credential-store`. It is session-wide,
so it serialises every thread of every Reldex process for the user's logon session, and it waits
up to 10 s before giving up with `Backend { code: 258 }` (`WAIT_TIMEOUT`). A holder that dies
mid-call leaves `WAIT_ABANDONED`, which is taken as acquired. With the mutex, 6,000 cycles over two
processes × four threads left nothing behind. The regression test (two processes × two threads,
120 keys) passes 10 of 10 and fails 5 of 5 with the mutex removed. **Residual, accepted:** the
mutex cannot serialise *other* applications that write their own credentials at the same instant.
A lost write then surfaces as `NotStored` and a prompt, which is safe. A lost delete leaves an
orphaned entry under a UUID that is never reused. It cannot attach to another profile, but the
password is still in the store. See the M3.2 hand-off below.

**Hygiene.** `put` hands `CredWriteW` the secret's own bytes without copying them. `get` copies the
blob into a vector with exact capacity (no reallocation, so no stray copy), wipes the system's
copy with `zeroize` before `CredFree`, and wipes bytes that fail UTF-8 decoding. A read entry is
wiped on drop unless it has moved into the `Secret`.

### S5 — `unsafe` in one file, fenced like the FFI boundary

Calling the Credential Manager is calling foreign functions, and there is no safe way to do that.
ADR-0003 D2 makes `crates/ffi` the product's single exception to `unsafe_code = "deny"`, and the
link probe is the other. This ADR adds a third, narrower one: **`crates/secrets/src/wincred.rs`
only**, which is `cfg(windows)`. It gets the same fence as D2. The module-level
`#![allow(unsafe_code, reason = …)]` is paired with denied `unsafe_op_in_unsafe_fn`,
`clippy::undocumented_unsafe_blocks` and `clippy::missing_safety_doc`. Every block wraps one call
and carries a `SAFETY:` comment. The module has no business logic: the target-name rule, the error
classification and UTF-8 decoding are plain functions or live in the safe modules.
`crates/ffi/tests/fences.rs` now accepts a *file* path as well as a crate directory and lists this
file, not the crate, so `unsafe` anywhere else in `reldex-secrets` still fails CI. The
alternatives were the `keyring` crate, which hides the same calls behind someone else's scheme, and
routing the calls through `crates/ffi`, which would put a platform service inside the Qt boundary
and make the core's credential code depend on the UI crate. Both are worse than one file of about
400 lines, tests included, that one reviewer can read.

### S6 — `Secret` wipes itself with `zeroize`

ADR-0002 accepted "`Secret` zeroing stays best-effort, no `zeroize` dependency for now; revisit in
the credential-storage ADR". Revisited here, and taken:

- `Secret::drop` calls `Zeroize` on its `Vec<u8>`. Its volatile writes may not be elided, and it
  covers the whole capacity. The previous `fill(0)` covered only the length and was a dead store
  immediately before a free, which an optimiser is entitled to remove.
- Cost: `reldex-db-driver-api` has one production dependency instead of zero. `zeroize` 1.9.0,
  Apache-2.0 OR MIT, `alloc` feature only, has no dependencies, exposes no `unsafe`, and was
  already compiled into every product build (via `aws-lc-rs`/`der` under the Oracle driver) at this
  version. It stays private: no `zeroize` type appears in the contract's API.
  `crates/secrets/tests/dependency_rules.rs` pins the contract's production dependencies to exactly
  `zeroize`.
- What it still does not promise, and the type's documentation says so: that no copy survives. A
  `&str` passed to `Secret::new` is copied and the source belongs to the caller. A `String` that
  reallocated while it was being built left its old buffer behind. The allocator, the page file and
  crash dumps are out of reach.

### S7 — Dependencies

`cargo tree -p reldex-secrets -e normal,build --prefix none` gives the same result on
`x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`, apart from `windows-sys`/`windows-link`
(Windows) and `libc` (Unix):

| Crate | Version | Licence | New to the graph? | Why |
| --- | --- | --- | --- | --- |
| `windows-sys` (`Win32_Foundation`, `Win32_Security_Credentials`, `Win32_System_Threading`), Windows only | 0.61.2 | MIT OR Apache-2.0 | **yes** (the lockfile already had 0.52.0 as an unused optional dependency of `ring`) | the four Credential Manager calls and the named mutex |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | no (via `chrono`) | `windows-sys`'s only dependency |
| `zeroize` (`alloc`) | 1.9.0 | Apache-2.0 OR MIT | no (via `aws-lc-rs`, `der`) | S6, and this crate's own buffers |
| `reldex-workspace`, `reldex-db-driver-api` | — | GPL-3.0-or-later | — | `CredentialKey`/`Profile`, `Secret`; `reldex-workspace` brings `rusqlite`/`uuid` (ADR-0006 P8), all already in the graph |

All are compatible with GPL-3.0-or-later. **`windows-sys`, not the `windows` crate named in the
owner's approval.** It is the same Microsoft `windows-rs` project under the same licence. The
approval's substance was "Windows Credential Manager through Microsoft's own bindings, not
`keyring`", and that holds. `windows-sys` is only the raw declarations this needs. `windows` adds
COM/WinRT wrapper layers (`windows-core`, `windows-result`, `windows-strings`) on top, which are
more to compile and more to audit, for four calls. Dev-dependency only: `serde_json` (the
dependency-rule test).

## Consequences

- **M2.11** exposes this over the C ABI. The composition root calls `platform_default()` once and
  keeps an `Arc<dyn CredentialStore>` on the workspace service thread. It carries
  `CredentialStoreKind`, `CredentialError`, `PromptReason` and `PasswordSource` as numeric enums.
  A password crosses only as bytes passed into a call, or handed out by a call that also offers a
  function to wipe and free them. `reldex.h` is unchanged by this ADR (ABI 3).
- **M3.2 (connection manager)** offers "save password" only when `kind().can_store()`, and
  otherwise saves the profile as "prompt each time". When a profile switches from "saved" to
  "prompt each time", it deletes the stored entry. When a profile is deleted, it deletes the entry
  too (after `Store::delete_profile`, treating `NotFound`/`Unavailable` as done). It shows
  `CredentialError`'s `Display`, which is value-free. Later, and optionally: a sweep that deletes
  `Reldex/profile/*` entries whose UUID no longer names a profile, which would also close S4's
  residual lost-delete case.
- **M3.3 (connect flow)** calls `resolve_password` on the workspace service thread and handles all
  three variants by name. On `PromptRequired(reason)` it tells the user why (for example "the
  saved password could not be read: …" for `StoreFailed`) and asks. If the user opts to save, it
  `put`s the typed password only after a successful connect, so a mistyped password is never
  saved. The password then goes to `connection_params` and nowhere else.
- **M3.7 (logging)**: `CredentialError`, `PromptReason`, `CredentialStoreKind` and the stores'
  `Debug` are value-free, and `PasswordSource`'s `Debug` prints `Secret(<redacted>)`. The M3.7
  log test should include a store round trip with a marker password among the inputs it scans for.
- **Packaging (M6.x)**: the uninstaller may offer to remove saved passwords by deleting every
  `Reldex/*` target (`cmdkey /list:Reldex/*`, then `cmdkey /delete:<target>`).
- **Later platforms**: Apple Keychain (macOS/iOS), Android Keystore and Linux Secret Service are
  new `CredentialStoreKind` variants and new `cfg`-gated modules. Each is a later task, and until
  one lands that platform prompts every time. Each will need its own `unsafe`/FFI decision, recorded
  as an amendment here.

## Alternatives considered

- **`keyring` crate.** Rejected for Phase 1, as §C.3 item 7 already recorded: on Windows it wraps
  the same four calls. Its target-name scheme and error type are its own, it would not have
  exposed the concurrent-writer loss S4 found, let alone fixed it, and its Linux path pulls in a
  D-Bus stack. It may still be evaluated for macOS/Linux later.
- **The `windows` crate.** Rejected in favour of `windows-sys`, same project and licence (S7).
- **A DPAPI-encrypted blob in the SQLite store, for platforms without a store.** Rejected: this is
  exactly the fallback the owner ruled out, and ADR-0006 P7 makes it impossible by construction.
- **`CRED_PERSIST_SESSION` or `CRED_PERSIST_ENTERPRISE`.** Rejected (S4).
- **Storing the database user name as the credential's `UserName`.** Rejected: it duplicates the
  profile and could disagree with it.
- **Credential attributes to namespace the entry.** Rejected: the target-name prefix already
  namespaces it and is what `cmdkey` and `CredEnumerateW` filter by. Attributes are invisible in
  both.
- **Returning `Result<PasswordSource, CredentialError>` from `resolve_password`.** Rejected (S3): it
  gives the caller an error path that can only ever end in the same prompt.
- **Only an in-process `Mutex` around the calls.** Measured sufficient within one process, but it
  leaves the two-process loss (a second Reldex window or instance, a test run) in place. The named
  mutex covers both at the cost of one kernel object per call.
- **Verify-by-read-back after each write.** Rejected: the in-process variant of the loss is
  invisible to reads until the process exits, so read-back cannot detect it.

## Evidence

`cargo test -p reldex-secrets` on Windows 11 (this machine): 15 unit tests, 9 Credential Manager
integration tests, 3 dependency-rule tests. On Linux/macOS, the unit tests that do not need Windows
and the dependency rules run, and `cargo clippy --target aarch64-linux-android -p reldex-secrets
--all-targets -- -D warnings` is clean here. The concurrency figures in S4 come from a scratch
harness (N threads × M put/get/delete cycles on fresh keys, then `cmdkey /list:Reldex/*`), which is
not committed. The committed regression test is `concurrent_reldex_processes_lose_no_update`.
