# Spike S6 — mobile cross-compile (Android / iOS)

**Status:** Complete. Kill criterion did **not** fire.
**ADR:** [`docs/decisions/0001-database-driver-strategy.md`](../../decisions/0001-database-driver-strategy.md),
"Platform viability" and the S6 row of the spike plan.
**Workflow:** [`.github/workflows/mobile-cross-compile.yml`](../../../.github/workflows/mobile-cross-compile.yml)
**Evidence PR:** [reldex/reldex#3](https://github.com/reldex/reldex/pull/3) (draft, opened only to trigger CI)

## What S6 asks, and what it deliberately does not answer

ADR-0001's S6 kill criterion: **"Either target fails to build and no provider swap
fixes it."** The question is narrow: does the core plus Oracle's thin driver — in
particular `rustls`'s default `aws-lc-rs` crypto provider, whose `aws-lc-sys`
compiles C and assembly — **build and link** for `aarch64-linux-android` and
`aarch64-apple-ios`, on ordinary GitHub-hosted runners with no local NDK or Xcode.

**Cross-compiling is not mobile support** (`AGENTS.md`; the `reldex-development`
skill's "Mobile work" section; `SPEC.md`). See "What this does NOT prove" below
for the long list of things a green cross-compile says nothing about. This
document only closes the S6 line item; it does not change the status of Phase 0
success criterion 7 in `docs/exec-plans/active/phase-0.md` (physical-device
evidence), which the lead updates separately.

## Environment

| | android-arm64 job | ios-arm64 job |
| --- | --- | --- |
| Runner image | `ubuntu-latest` | `macos-latest` (Apple Silicon) |
| rustc | 1.98.1 (`48a229cea`, 2026-09-01) | 1.98.1 (`48a229cea`, 2026-09-01) |
| cargo | 1.98.1 (`797e8a9bc`, 2026-08-05) | 1.98.1 (`797e8a9bc`, 2026-08-05) |
| Rust targets added | `aarch64-linux-android` | `aarch64-apple-ios`, `aarch64-apple-ios-sim` |
| Android NDK | `ANDROID_NDK_LATEST_HOME` = r29 (`29.0.14206865`); the image's default `ANDROID_NDK_HOME` was r27 (`27.3.13750724`) — the workflow explicitly repoints `ANDROID_NDK_HOME` at the r29 install before building | n/a |
| Android API level | 26 (arbitrary, comfortably-modern choice; nothing in this dependency graph forced it) | n/a |
| Build tool | `cargo-ndk` 4.1.2 (`cargo install cargo-ndk --locked`) | plain `cargo build --target ...` (no extra tool needed) |
| Xcode | n/a | Xcode 26.6, build `17F113` |
| iOS SDK | n/a | `iPhoneOS26.5.sdk` / `iPhoneSimulator26.5.sdk` |
| iOS deployment target | n/a | `IPHONEOS_DEPLOYMENT_TARGET=15.0` — set to clear a link error (see below), not a decision about Reldex's real minimum iOS version |

## Exact commands

Package set (`$S6_PACKAGES` in the workflow), built together in one invocation per
target so cargo shares the resolved dependency graph:

```text
-p reldex-db-driver-api -p reldex-db-core -p reldex-driver-oracle-thin \
-p reldex-mobile-link-check -p reldex-core-poc
```

- Android (arm64-v8a / `aarch64-linux-android`, API 26):
  `cargo ndk -t arm64-v8a -P 26 build --release $S6_PACKAGES`
- iOS device (`aarch64-apple-ios`):
  `cargo build --release --target aarch64-apple-ios $S6_PACKAGES`
- iOS simulator (`aarch64-apple-ios-sim`):
  `cargo build --release --target aarch64-apple-ios-sim $S6_PACKAGES`
- Provider-swap evidence, run on both jobs against their own target:
  `cargo tree -e features --target <target> -i rustls`

`crates/mobile-link-check` (`reldex-mobile-link-check`, `publish = false`) is a
new, disposable workspace member added for this spike: a `staticlib`+`cdylib`
whose one `extern "C"` function references `OracleThinDriver::new()`,
`install_default_crypto_provider()`, and `reldex-db-core::SessionManager`, so
building it forces the **linker** — not just the compiler — to resolve the whole
dependency graph, `aws-lc-sys` included. It is a build/link probe, not product
code (see its module docs). `reldex-core-poc` (the Phase 0 CLI harness) was
built alongside it as a plain executable, which is meaningful on Android in
particular since it can in principle be `adb push`ed and run.

## Per-target results

| Target | Compile | Link | `libmobile_link_check` | `reldex-core-poc` |
| --- | --- | --- | --- | --- |
| `aarch64-linux-android` | Pass | Pass | `.so` 2,676,128 B unstripped / 2,285,896 B stripped (`llvm-strip --strip-all`); `.a` 15,390,220 B (not stripped) | 4,849,112 B unstripped / 3,932,024 B stripped |
| `aarch64-apple-ios` (device) | Pass | Pass | `.dylib` 2,338,312 B; `.a` 10,692,824 B unstripped / 6,990,520 B after `strip -S` | 4,258,352 B (not stripped) |
| `aarch64-apple-ios-sim` (simulator) | Pass | Pass | `.dylib` 2,373,200 B; `.a` 10,716,016 B (not stripped) | 4,308,224 B (not stripped) |

Rlib sizes (release, unstripped — intermediate build products, not shipped
artifacts): `reldex-db-driver-api` ~1.00–1.03 MB, `reldex-db-core` ~0.95–0.99 MB,
`reldex-driver-oracle-thin` ~0.73–0.77 MB on both platforms; near-identical
across Android/iOS/iOS-sim as expected for the same source compiled three times.

`file` output confirms genuine target binaries, not host artifacts:

- Android: `libmobile_link_check.so` → `ELF 64-bit LSB shared object, ARM aarch64
  ... dynamically linked, not stripped`; `reldex-core-poc` → `ELF 64-bit LSB pie
  executable, ARM aarch64 ... interpreter /system/bin/linker64` (Android's
  bionic dynamic linker — this is unambiguously an Android binary, not a generic
  Linux one).
- iOS: `libmobile_link_check.dylib` → `Mach-O 64-bit dynamically linked shared
  library arm64`; `libmobile_link_check.a` → `current ar archive`;
  `reldex-core-poc` → `Mach-O 64-bit executable arm64`.

Stripping the iOS `.a` (`strip -S`) worked and was not a no-op — it dropped the
static archive from 10.69 MB to 6.99 MB (34.6%), stripping local/debug symbols
from its member object files. `libmobile_link_check.a` was not stripped on
Android; `llvm-strip` was only exercised there against the `.so` and the
`reldex-core-poc` binary, which is what the task asked for.

## `aws-lc-sys`: no extra tools were needed

Neither job installed anything beyond `cargo-ndk` itself (Android) — no
`cmake`, no `bindgen`/`libclang`, no NASM, no Go. The `cmake` *Rust crate*
appears in the dependency graph (`aws-lc-sys`'s own build-dependency list) and
was compiled on both platforms, but nothing in either job's log shows the actual
`cmake` binary being invoked (no "Configuring done", no CMake output at all) —
consistent with `aws-lc-rs`'s documented non-FIPS default needing "CMake never,
bindgen never, Go never" (quoted in ADR-0001). The Android NDK's own
clang/ar (via `cargo-ndk`) and Xcode's clang handled all the C/assembly
compilation directly.

## Provider-swap finding (the `ring` question)

`cargo tree -e features --target <target> -i rustls` (run on both jobs) shows
the same inversion on Android and iOS:

```text
rustls v0.23.45
├── rustls feature "aws-lc-rs"
│   └── rustls feature "aws_lc_rs"
│       ├── rustls feature "default"
│       │   ├── oracledb v26.0.0-beta.3
│       │   │   └── oracledb feature "default"
│       │   │       └── reldex-driver-oracle-thin v0.0.0
...
```

`oracledb` depends on `rustls` with its **default features on** (no
`default-features = false`), and Cargo feature unification is additive: once
anything in the graph turns a feature on, it is on for the whole build. Nothing
in `reldex-driver-oracle-thin`'s own `Cargo.toml` can turn `aws-lc-rs` back off
for `oracledb`'s copy of `rustls` — that would require `oracledb` itself to
expose a `default-features = false` + explicit-provider knob (it does not,
per ADR-0001's own upstream research), or a fork/patch of `oracledb` or a
`[patch]`/vendored `rustls`. **A provider swap to `ring` is not available to us
without forking** — this matches ADR-0001's own analysis; S6 did not need to
exercise this path since both targets built and linked with `aws-lc-rs` as-is.

## Verdict vs. the S6 kill criterion

**Kill criterion does not fire.** Both `aarch64-linux-android` and
`aarch64-apple-ios` (plus `aarch64-apple-ios-sim`) build and link successfully
in CI, including a real `cdylib`/`staticlib` link (not just `cargo check`) and,
on Android, a plain executable.

Two real problems surfaced on the first CI attempt
([run 35435964035](https://github.com/reldex/reldex/actions/runs/35435964035),
both jobs failed) and were fixed on the second
([run 35436219873](https://github.com/reldex/reldex/actions/runs/35436219873),
[android-arm64 job](https://github.com/reldex/reldex/actions/runs/35436219873/job/105879212488),
[ios-arm64 job](https://github.com/reldex/reldex/actions/runs/35436219873/job/105879212673),
both green) — neither was a fundamental incompatibility:

1. **Android:** `cargo-ndk`'s platform/API-level flag is `-P` (uppercase); the
   workflow used lowercase `-p`, which is not one of `cargo-ndk`'s own flags, so
   it fell through to the forwarded `cargo` args and `cargo` read `-p 26` as
   `--package 26`, failing with `unknown package: 26`. A one-character
   workflow fix.
2. **iOS:** linking failed with `Undefined symbols for architecture arm64:
   "___chkstk_darwin"` from `aws-lc-sys`'s compiled assembly (`bcm.o`). Root
   cause: rustc's own default `aarch64-apple-ios` link target is a very old
   minimum (`arm64-apple-ios10.0.0`), while the runner's Xcode 26.6 toolchain
   compiled `aws-lc-sys`'s C/assembly against its current 26.5 SDK with no
   minimum of its own; the SDK's `libSystem` stub only exposes
   `___chkstk_darwin` (a stack-probe helper) to a new-enough target minimum.
   Setting `IPHONEOS_DEPLOYMENT_TARGET=15.0` for the job resolved it. This is a
   deployment-target/toolchain configuration detail of *this CI runner's* very
   new Xcode, not evidence against mobile viability.

Given the fix, `aws-lc-rs`/`aws-lc-sys` behave on both targets exactly as
`aws-lc-rs`'s own platform-support table claims (build+tested for
`aarch64-linux-android` and `aarch64-apple-ios`) — this is now Reldex's own
evidence for that claim, not just upstream's.

## What this does NOT prove

This is a cross-compile-and-link check only. It says nothing about:

- Connecting to a real Oracle database from an Android or iOS device or
  simulator/emulator, over any transport (plaintext or TCPS/TLS).
- SQL or PL/SQL execution, transactions, SAVEPOINT/ROLLBACK, or query
  cancellation on-device.
- LOB streaming, NUMBER/DATE/TIMESTAMP fidelity, or any of the S1–S5/S7–S9
  behavioral spikes, on-device.
- TLS handshake success on-device (the crypto provider was only *installed*,
  never used to negotiate a real connection, by `reldex_link_check`).
- Background/resume behavior, or lost-session/reconnect semantics, under a real
  mobile OS's process lifecycle (a mobile OS can suspend or kill the app
  mid-connection in ways a desktop process never experiences).
- App packaging, code signing, or store constraints (Google Play, App Store)
  — no `.apk`/`.aab`, no `.ipa`, no entitlements, no provisioning profile were
  produced.
- iOS's local-network permission prompt (required before an app can reach a
  LAN database), which only appears in a real app on a real device.
- Whether `reldex_link_check()` (or `reldex-core-poc`) actually *runs*
  correctly on-device — only that it *links*. Neither binary was executed on
  this run: the Android binaries are cross-compiled ELF objects a Linux
  x86_64 runner cannot execute, and the iOS binaries are unsigned Mach-O
  objects a Mac cannot run outside a simulator without a provisioning step
  this spike did not attempt.
- Emulator/simulator behavior counting as validation: `AGENTS.md` and the
  `reldex-development` skill are explicit that "mobile support is claimed from
  emulator/simulator-only results" is an incomplete change. Nothing here,
  including the `aarch64-apple-ios-sim` build, changes that; the simulator
  target was built only because doing so was cheap alongside the device build.

Phase 0 success criterion 7 (`docs/exec-plans/active/phase-0.md`) remains
**documented feasibility evidence, not physical-device evidence** after this
spike.

## Next step toward physical-device validation

**Android first** — cheaper and faster to get real hardware evidence:

1. Build a minimal native harness: either (a) an `adb push`-able variant of
   `reldex-core-poc` invoked over `adb shell` against a database reachable from
   the device's network, or (b) a trivial one-Activity Android app that loads
   `libmobile_link_check.so` (or a real driver entry point) via JNI and calls
   into it, logging the result to `adb logcat`.
2. What the owner needs to provide/decide: a physical Android device (arm64,
   Android 8.0/API 26+ to match this spike's chosen level) with USB debugging
   enabled, and network access from that device to a real Oracle instance (the
   existing Phase 0 test database, or a reachable equivalent — direct-connect
   from a mobile network to an on-prem/VPN-only database may itself need a
   decision).
3. Exercise, in order of ADR-0001/`reldex-development`'s "Mobile work" list:
   connect, a simple SQL round trip, a transaction, cancellation, a LOB, and
   TCPS — the same shape as spikes S1–S5/S7–S9, but run from the device.

**iOS second** — needs more setup:

1. A Mac with Xcode (available — this spike's `macos-latest` CI confirms the
   toolchain; a physical run additionally needs a local Mac, not just CI).
2. An Apple Developer account. A free personal-team signing identity can run a
   debug build on a personally-owned device for local testing (7-day
   provisioning renewal); anything longer-lived, or distribution to another
   tester, needs the paid Apple Developer Program.
3. A physical iPhone (or iPad) to run on, plus handling the iOS local-network
   permission prompt in a real app shell (a bare `staticlib` cannot exercise
   this — it needs an actual app target).
4. The same minimal-harness approach as Android: a one-screen SwiftUI/UIKit app
   linking `libmobile_link_check.a` (or the real driver) and calling into it,
   run via Xcode's "Run" onto the device.

## Process notes

- 3 pushes used against this task's 5-push budget: the first added the
  workflow + `crates/mobile-link-check`, which failed both jobs; the second
  fixed the `cargo-ndk` flag and the iOS deployment target, and both jobs
  passed; the third added this document (no further workflow/code change).
- `ci.yml` (fmt/clippy/test on all three OSes) stayed green throughout,
  including with `reldex-mobile-link-check` in the workspace — see the
  [passing run alongside the second attempt](https://github.com/reldex/reldex/actions/runs/35436219876).

## Doc updates the lead still needs to make

Not edited here per this task's scope (the lead syncs these):

- `docs/decisions/0001-database-driver-strategy.md`: the "Results at a glance"
  line for S6 currently reads "**Not run** — Android/iOS cross-compile needs
  the Android NDK (owner approval to download) and a macOS host" — needs
  updating to reflect this pass, run on GitHub-hosted runners with no local
  NDK/Xcode needed.
- `docs/exec-plans/active/phase-0-spike-results.md`: its own "S6" section
  currently says "S6 (metadata queries) belongs to a later slice and was not
  attempted" — this text describes a *different* S6 than ADR-0001's actual S6
  (cross-compile checks) and appears to be stale/mislabeled from an earlier
  spike-numbering draft; it should be replaced with this spike's result.
- `docs/exec-plans/active/phase-0.md`: success-criterion-7 status row ("Spike
  S6 (cross-compile) has not run") needs updating to point at this document,
  while still marking physical-device evidence as outstanding.
- `Task.html`: refresh the S6 row/status and activity log per `AGENTS.md`'s
  dashboard rule.
