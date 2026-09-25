# 0007 — Credential storage: `CredentialStore`, the Windows Credential Manager, no fallback

**Status:** Accepted for S1–S4 and S8. They implement the owner decision of 2026-09-20
(`docs/exec-plans/active/phase-1.md` §C.3 item 7): Windows Credential Manager for Phase 1, and when
no store is available the user is prompted every time, with no plaintext fallback ever. **S5, S6
and S7 are provisionally accepted by the lead, pending owner confirmation** (see "Owner review"
below). S7 replaces the `windows` crate named in that approval with `windows-sys`. S5 and S6 settle
the two points earlier ADRs deferred to "the credential-storage ADR": `unsafe` outside `crates/ffi`,
and whether `Secret` uses `zeroize` (ADR-0002, accepted item 4). This ADR also answers
`ARCHITECTURE.md` §13 item 9.
**Date:** 2026-09-25; revised the same day after the independent review of PR #36 (entry format
S8, mechanism and lock corrections in S4, hand-off rules in Consequences).
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
- **`&self` and `Send + Sync`.** One instance is shared (`Arc<dyn CredentialStore>`); the Windows
  backend serialises its own calls (S4).
- **Blocking.** Every call can block. Windows calls go to the local security authority, and a
  future Keychain call can show a system prompt. The calls belong on the workspace service thread,
  never the UI thread (`ARCHITECTURE.md` §6) and never a session worker.
- **`CredentialError` is value-free.** Its variants are:
  - `Unavailable`: no store, or the store is unusable from this logon session.
  - `NotFound`.
  - `Denied`: the credential call itself was refused.
  - `TooLarge { max_bytes }`: nothing written; the limit is per backend.
  - `InvalidSecret`: the password contains a control character; nothing written (S8).
  - `Malformed`: an entry Reldex did not write (S8).
  - `Locked { code }`: the store's lock could not be taken (S4).
  - `Backend { code: i64 }`: the platform's code preserved; a Win32 `u32` or an Apple `OSStatus`
    `i32` both fit.

  No variant carries a key, a target name or a secret, and exact `Debug`/`Display` renderings are
  tested. `#[non_exhaustive]`.
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
| password, saved in the store | holds a Reldex entry | `FromStore(secret)` |
| | holds nothing (or a store answers `NotFound`) | `PromptRequired(NotStored)` |
| | `Unavailable` | `PromptRequired(StoreUnavailable)` |
| | any other error (`Malformed`, `Locked`, `Denied`, `Backend`, …) | `PromptRequired(StoreFailed(error))` |

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
| target name | `Reldex/profile/<uuid>`, lowercase hyphenated | `Reldex/` namespaces every entry, so `cmdkey /list:Reldex/*` lists exactly Reldex's entries and an uninstaller or the sweep (M2.14) can delete exactly those; stable forever, since changing it orphans every saved password |
| blob | the S8 entry: `RLDX`, `0x01`, then the password's UTF-8 bytes | so an entry Reldex did not write is never sent to a database (S8) |
| limit | 2,555 password bytes (`CRED_MAX_CREDENTIAL_BLOB_SIZE` 2,560 less the 5-byte prefix), `WindowsCredentialManager::MAX_SECRET_BYTES` | a longer password is refused with `TooLarge` before any write (tested at 2,555 and 2,556) |
| persistence | `CRED_PERSIST_LOCAL_MACHINE` | see below |
| comment | `Reldex credential v1` | shown in Control Panel next to the target; names the format for a human (the blob prefix is what the code checks) |
| user name / alias / attributes | none | the database user name is in the profile; a second copy could only disagree |

**Persistence scope.** `CRED_PERSIST_SESSION` drops the entry at logoff, so "save password" would
stop working after every reboot. That is the promise the user made a choice about, broken silently.
`CRED_PERSIST_ENTERPRISE` roams the entry with the user's domain profile to every machine it
reaches, which is wider exposure than "save password on this computer" asks for. It would also
disagree with the profile store the entry belongs to, which is deliberately local
(`%LOCALAPPDATA%`, not roaming, ADR-0006 P5). A profile copied to another machine prompts there,
and that is correct. `CRED_PERSIST_LOCAL_MACHINE` is this user on this machine, across logons, and
is not roamed. The lead confirmed this choice after the review, despite the concurrency cost below.

**Concurrent writers (measured; fixed between Reldex callers).** On Windows 11 (10.0.26200), with
`CRED_PERSIST_LOCAL_MACHINE`, the Credential Manager lost updates when calls wrote or deleted
*different* targets at the same moment:

- **Two threads in one process, no lock.** A read straight after each delete was always clean, yet
  deleted entries reappeared *later, while the process was still running* (the review's run: 76 of
  600 cycles; mine, counted with `cmdkey` after exit: 14 of 600).
- **Two processes, no lock.** Additionally, an entry that had just been written read back as absent
  in its writer, and its delete then failed with `NotFound` (18 entries left behind and 5 lost
  writes in 600 cycles).
- **Control run with `CRED_PERSIST_SESSION`** (review): 0 of 600 lost, at about 0.25 ms per call
  against about 20 ms.

So the race is in how the on-disk credential file is rewritten and reloaded, not in the calls
themselves. Session persistence avoids the race but loses saved passwords at logoff, so
local-machine persistence stays. Others writing through the same API have hit the same behaviour;
in particular, a process-wide lock alone has been shown not to be enough.

**The lock.** Every call holds a named mutex, `Local\Reldex.CredentialStore.<user SID>`:

- **One per user** in the logon session's namespace. The SID is read once from the process token.
  A process-wide `Mutex` was measured insufficient across processes (review: 5 lost writes and 74
  leftovers with two processes; the named lock: 0 of 1,200).
- **Created with its own security descriptor,** `D:(A;;0x100001;;;<SID>)S:(ML;;NW;;;ME)`: the user
  may wait on it and release it, nothing more, and the label is medium with no write-up.
- **Opened with exactly `SYNCHRONIZE | MUTEX_MODIFY_STATE`,** not all-access, so an elevated and a
  normal process of the same user can share it whichever created it. That is reasoned, not tested:
  the test would need a UAC prompt.
- **Waits at most 2 s.** Any failure to take the lock is `CredentialError::Locked { code }`, never
  `Denied`:
  - `258` (`WAIT_TIMEOUT`): another process held it past the wait.
  - `5` (`ERROR_ACCESS_DENIED`): an object of that name refuses the two rights, e.g. one squatted
    with an empty DACL. This fails at once, and the connect flow prompts (tested with a real
    squatter process).
- **A holder that dies mid-call** leaves `WAIT_ABANDONED`, which is taken as acquired.

With the lock, 6,000 cycles over two processes × four threads left nothing behind. The regression
test (two processes × two threads, 120 keys) passes 10 of 10 and fails 5 of 5 with the lock
removed.

**Latency, uncontended.** Measured on this machine, with 40 other credentials in the user's set,
one Reldex entry at a time, n = 200:

- a write (`put`): 12.5 ms median;
- the next call after a write: about 11.7 ms, because the file rewrite lands on it;
- a read with no write pending: 0.23 ms;
- a delete: 0.43 ms.

With 200 Reldex entries present, `put` and `delete` take 44 ms median, because the whole file is
rewritten on every write. The review measured 0.3 ms per read and about 14 ms per write. The 2 s
wait is more than 40 times the worst of these.

**Residual, accepted.** The lock cannot serialise *other* applications that write their own
credentials at the same instant. The review ran a locked process beside one unlocked writer: 2
writes lost and 14 of 400 deletes undone. A lost write surfaces as `NotStored` and a prompt, which
is safe. A lost delete leaves an orphaned entry under a UUID that is never reused. It cannot attach
to another profile, but the password is still in the store. The planned **M2.14 orphan sweep**
(Consequences) removes it.

A second leftover was found while testing S8. If another tool has written an entry under a Reldex
name with a *different* persistence (`cmdkey` uses enterprise persistence), a Reldex `put` over it
and a later `delete` can leave the other tool's copy behind; it reappears after the process exits
(5 of 6 runs). Reldex's `delete` of such an entry, without a `put` over it first, left nothing (0 of
20). The leftover is the other tool's entry (its user name shows), so it reads as `Malformed` and
leads to a prompt; it is never sent to a database. The M2.14 sweep removes it too, and the
integration tests never `put` over a `cmdkey` entry.

**Hygiene.** `put` builds the S8 entry in one exact-capacity buffer that is wiped when dropped.
`get` copies the blob into a vector with exact capacity (no reallocation, so no stray copy), wipes
the system's copy with `zeroize` before `CredFree`, and wipes bytes that fail decoding. A read entry
is wiped on drop unless it has moved into the `Secret`.

### S5 — `unsafe` in one file, fenced like the FFI boundary

*Provisionally accepted by the lead, pending owner confirmation.*

Calling the Credential Manager, and taking the lock around it, means calling foreign functions:
`Cred*`, the token/SID/security-descriptor calls and the mutex calls. There is no safe way to do
that. ADR-0003 D2 makes `crates/ffi` the product's single exception to `unsafe_code = "deny"`, and
the link probe is the other. This ADR adds a third, narrower one: **`crates/secrets/src/wincred.rs`
only**, which is `cfg(windows)`. It gets the same fence as D2:

- The module-level `#![allow(unsafe_code, reason = …)]` is paired with denied
  `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and `clippy::missing_safety_doc`.
- Every block wraps one call and carries a `SAFETY:` comment.
- The module has no business logic. The entry format is the safe `format` module (S8), and the
  target-name rule and error classification are plain functions.
- `crates/ffi/tests/fences.rs` accepts a *file* path as well as a crate directory, and lists this
  file, not the crate. It detects `allow(…)` and `expect(…)` naming `unsafe_code` anywhere in the
  list, so `unsafe` anywhere else in `reldex-secrets` still fails CI.

The alternatives were the `keyring` crate, which hides the same calls behind someone else's scheme,
and routing the calls through `crates/ffi`, which would put a platform service inside the Qt
boundary and make the core's credential code depend on the UI crate. Both are worse than one
reviewable file.

### S6 — `Secret` wipes itself with `zeroize`

*Provisionally accepted by the lead, pending owner confirmation.*

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

*Provisionally accepted by the lead, pending owner confirmation (the `windows-sys` substitution).*

`cargo tree -p reldex-secrets -e normal,build --prefix none` gives the same result on
`x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`, apart from `windows-sys`/`windows-link`
(Windows) and `libc` (Unix):

| Crate | Version | Licence | New to the graph? | Why |
| --- | --- | --- | --- | --- |
| `windows-sys` (`Win32_Foundation`, `Win32_Security`, `Win32_Security_Authorization`, `Win32_Security_Credentials`, `Win32_System_Threading`), Windows only | 0.61.2 | MIT OR Apache-2.0 | **yes** (the lockfile already had 0.52.0 as an unused optional dependency of `ring`) | the Credential Manager calls, the user's SID, the lock's security descriptor and the named mutex |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | no (via `chrono`) | `windows-sys`'s only dependency |
| `zeroize` (`alloc`) | 1.9.0 | Apache-2.0 OR MIT | no (via `aws-lc-rs`, `der`) | S6, and this crate's own buffers |
| `reldex-workspace`, `reldex-db-driver-api` | — | GPL-3.0-or-later | — | `CredentialKey`/`Profile`, `Secret`; `reldex-workspace` brings `rusqlite`/`uuid` (ADR-0006 P8), all already in the graph |

All are compatible with GPL-3.0-or-later. **`windows-sys`, not the `windows` crate named in the
owner's approval.** It is the same Microsoft `windows-rs` project under the same licence. The
approval's substance was "Windows Credential Manager through Microsoft's own bindings, not
`keyring`", and that holds. `windows-sys` is only the raw declarations this needs. `windows` adds
COM/WinRT wrapper layers (`windows-core`, `windows-result`, `windows-strings`) on top, which are
more to compile and more to audit. Dev-dependency only: `serde_json` (the dependency-rule test).

### S8 — The stored entry is versioned, and anything else is not a password

*Lead decision (b) after the review, taken before anyone has saved a password.*

The first version stored the bare UTF-8 password and returned any stored bytes that happened to be
valid UTF-8. An entry created under Reldex's name by anything else would then be sent to the
database as the password. The review demonstrated this with `cmdkey /generic:Reldex/profile/<uuid>
/pass:Hunter2x`, which stores UTF-16LE: 16 bytes, valid UTF-8 with NULs in it. Thai "กข" becomes
`01 0E 02 0E`, which is valid UTF-8 with no NUL. A wrong password counts towards the account's
failed-login limit (Oracle's default is 10), so repeated automatic attempts can lock it.

Now every entry is:

```text
"RLDX" (4 bytes)  0x01 (version)  password, UTF-8, no control character (U+0000–U+001F)
```

- **Reading.** `get` decodes only that shape. A missing or different magic, any other version (a
  future `0x02` included), or a payload that is not UTF-8 or contains a control byte, is
  `Malformed`. `resolve_password` turns that into `PromptRequired(StoreFailed(Malformed))`, and the
  entry is never sent to a database. A control byte cannot occur inside a UTF-8 multi-byte sequence,
  so a byte check is a character check.
- **Writing.** `put` refuses a password containing a control character with `InvalidSecret` and
  writes nothing. A round trip therefore never turns a saved password into `Malformed`. No database
  password Reldex supports needs one, and it can still be typed at every connect. The in-memory test
  store applies the same rule.
- **Limit and comment.** The prefix costs 5 bytes of the 2,560-byte blob, so the maximum password
  is 2,555 bytes. The comment is `Reldex credential v1`, and the user name stays NULL.
- **Portable.** The format lives in the platform-neutral `format` module, tested on every CI
  runner, so a later Keychain or Keystore backend can use the same entry shape. A later format is a
  new version byte, and this build reads version 1 only.

## Owner review

- **S5.** A third place allowed `unsafe`: one Windows-only file, fenced like `crates/ffi`.
- **S6.** The driver contract's first production dependency, `zeroize`.
- **S7.** `windows-sys` instead of the `windows` crate named in §C.3 item 7: the same project and
  licence, raw declarations only.

These are provisionally accepted by the lead and implemented. Reverting any of them is contained:
S5 would move the calls behind `keyring` or into `crates/ffi`; S6 is one `Drop` impl and one
dependency line; S7 is the import paths in one file.

## Consequences

- **M2.11** exposes this over the C ABI. The composition root calls `platform_default()` once and
  keeps an `Arc<dyn CredentialStore>` on the workspace service thread. It carries
  `CredentialStoreKind`, `CredentialError` (including `InvalidSecret`, `Malformed` and `Locked`),
  `PromptReason` and `PasswordSource` as numeric enums. A password crosses only as bytes passed
  into a call, or handed out by a call that also offers a function to wipe and free them.
  `reldex.h` is unchanged by this ADR (ABI 3).
- **Rules for saving, clearing and using a stored password.** These bind M3.2 and M3.3.
  - **Write order.** Store `put` first, then the profile's `PasswordStorage` flag. If `put` fails
    (`InvalidSecret`, `TooLarge`, `Locked`, `Denied`, `Backend`), the flag stays "prompt each
    time" and the UI says so. A profile never claims a password the store does not hold.
  - **Clearing.** When a profile switches to "prompt each time" or is deleted, the entry is
    deleted. `NotFound`/`Unavailable` count as done. If the delete fails (`Denied`, `Backend`,
    `Locked`), the flag still flips to "prompt each time", the leftover entry is reported to the
    user, and it is left for the sweep (M2.14).
  - **Authentication failure with a stored password.** If the database refuses a password that came
    `FromStore`, the next connect prompts, and the UI offers to update the saved password. **A stored
    password is never retried automatically.** Each retry counts towards the account's lockout limit.
  - **After a prompt.** If the user opts to save, the typed password is `put` only after a
    successful connect, so a mistyped password is never saved.
- **M3.2 (connection manager)** offers "save password" only when `kind().can_store()`, and
  otherwise saves the profile as "prompt each time". It follows the write-order and clearing rules
  above, and shows `CredentialError`'s value-free `Display`.
- **M3.3 (connect flow)** calls `resolve_password` on the workspace service thread and handles all
  three variants by name. On `PromptRequired(reason)` it tells the user why (for example "the
  saved password could not be read: …" for `StoreFailed`) and asks. It follows the
  authentication-failure and after-a-prompt rules above. The password goes to `connection_params`
  and nowhere else.
- **M2.14 (planned): credential orphan sweep.** Enumerate `Reldex/profile/*` (`CredEnumerateW` with
  that filter) and delete entries whose UUID names no profile. It runs at startup and after a profile
  is deleted, reports counts, and never touches an entry outside the namespace. It closes S4's
  residual lost-delete case and the leftover of a failed clear.
- **M3.7 (logging)**: `CredentialError`, `PromptReason`, `CredentialStoreKind` and the stores'
  `Debug` are value-free, and `PasswordSource`'s `Debug` prints `Secret(<redacted>)`. The M3.7
  log test should include a store round trip with a marker password among the inputs it scans for.
- **Packaging (M6.x)**: the uninstaller may offer to remove saved passwords by deleting every
  `Reldex/*` target (`cmdkey /list:Reldex/*`, then `cmdkey /delete:<target>`).
- **Later platforms**: Apple Keychain (macOS/iOS), Android Keystore and Linux Secret Service are
  new `CredentialStoreKind` variants and new `cfg`-gated modules that reuse the S8 entry format.
  Each is a later task, and until one lands that platform prompts every time. Each will need its own
  `unsafe`/FFI decision, recorded as an amendment here.

## Alternatives considered

- **`keyring` crate.** Rejected for Phase 1, as §C.3 item 7 already recorded: on Windows it wraps
  the same calls. Its target-name scheme and error type are its own, it would not have exposed the
  concurrent-writer loss S4 found, let alone fixed it, and its Linux path pulls in a D-Bus stack.
  It may still be evaluated for macOS/Linux later.
- **The `windows` crate.** Rejected in favour of `windows-sys`, same project and licence (S7).
- **A DPAPI-encrypted blob in the SQLite store, for platforms without a store.** Rejected: this is
  exactly the fallback the owner ruled out, and ADR-0006 P7 makes it impossible by construction.
- **`CRED_PERSIST_SESSION`.** It avoids the concurrent-writer race (S4's control run) but forgets
  saved passwords at logoff. Rejected (lead decision (c)).
- **`CRED_PERSIST_ENTERPRISE`.** Rejected (S4).
- **Storing the database user name as the credential's `UserName`.** Rejected: it duplicates the
  profile and could disagree with it.
- **Credential attributes to namespace or mark the entry.** Rejected: the target-name prefix already
  namespaces it and is what `cmdkey` and `CredEnumerateW` filter by, and attributes are invisible in
  both. The blob prefix (S8) is the marker, because it travels with the bytes.
- **Accepting any UTF-8 blob.** This was the first version, rejected by the review (S8).
- **Returning `Result<PasswordSource, CredentialError>` from `resolve_password`.** Rejected (S3): it
  gives the caller an error path that can only ever end in the same prompt.
- **Only an in-process `Mutex` around the calls.** Enough within one process, but measured
  insufficient across processes (S4). The named lock costs one kernel object per call.
- **Reporting a lock failure as `Denied` or `Backend`.** Rejected: "the credential store refused
  access" would misreport a squatted or held lock. `Locked { code }` says what happened.
- **Verify-by-read-back after each write.** Rejected: a read straight after the call is clean even
  when the update is later lost, so read-back cannot detect it.

## Evidence

`cargo test -p reldex-secrets` on Windows 11 (this machine), looped 10 times, all clean:

- **22 unit tests.** The entry format, including what `cmdkey` writes and a future version, and the
  control-character rule. Raw entries without the v1 prefix read back as `Malformed`. The stored
  entry is `RLDX\x01…`, `CRED_PERSIST_LOCAL_MACHINE`, with the comment `Reldex credential v1`. The
  lock name carries the SID. A squatted lock gives `Locked { code: 5 }`, and a held lock gives
  `Locked { code: 258 }` after about 2 s. The trait contract runs against the memory store and the
  real store. The `resolve_password` truth table covers 3 authentication modes × 12 store states.
- **11 Credential Manager integration tests.** Among them:
  - `an_entry_written_by_another_tool_is_malformed_and_prompts`: `cmdkey` "Hunter2x" and "กข" are
    both `Malformed` and both lead to a prompt.
  - `a_password_with_a_control_character_is_refused_and_nothing_is_written`.
  - The 2,555/2,556-byte limit.
  - `concurrent_reldex_processes_lose_no_update`.
- **`tests/lock_squat.rs`.** A PowerShell process creates the real lock with an empty DACL. `get`,
  `put` and `delete` are refused at once with `Locked { code: 5 }`, and the connect flow prompts.
  After the squatter lets go, the store works again.
- **3 dependency-rule tests.**

On Linux/macOS, the platform-neutral tests (format, resolve, memory, errors) and the dependency
rules run. `cargo clippy --target aarch64-linux-android -p reldex-secrets --all-targets -- -D
warnings` is clean here, and the Android/iOS cross-compile workflow now builds `reldex-workspace`
and `reldex-secrets`. The concurrency and latency figures in S4 come from scratch harnesses (N
threads × M put/get/delete cycles on fresh keys, then `cmdkey /list:Reldex/*`), which are not
committed. Figures credited to the review are the independent reviewer's own runs.
