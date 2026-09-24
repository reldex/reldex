# Phase 0 — Android physical-device validation

**Date:** 2026-09-19 (first connection) and 2026-09-20 (full run).
**Status:** Pass — 7 of 7 checks, on a physical ARM64 Android phone.
**Criterion:** [`phase-0.md`](phase-0.md) success criterion 7, Android half.
**Follows:** [`phase-0-s6-mobile-cross-compile.md`](phase-0-s6-mobile-cross-compile.md)
(cross-compile only), whose "Next step toward physical-device validation" this
is.
**Harness:** `crates/reldex-core-poc/src/bin/reldex-device-check.rs`
**Runner:** [`tools/android-device/`](../../../tools/android-device/)
**iOS:** unchanged — no iOS device evidence exists. See "Criterion 7 after this
run".

## What this run was for

S6 proved the stack *compiles and links* for `aarch64-linux-android`. It said
nothing about whether the result **runs**, or whether it can reach a database.
This run answers that: the same Rust core and driver stack the desktop uses —
`reldex-db-driver-api` → `reldex-db-core` → `reldex-driver-oracle-thin` →
`oracledb`, pure Rust, `rustls` with `aws-lc-rs` — executing on physical ARM64
Android hardware, talking to the Phase 0 Oracle 19c test database.

Every database call goes through **`reldex-db-core`**, not straight to the
driver, so each one runs on a session's dedicated worker thread (ADR-0002
D1/D2). A green run is therefore evidence for the core's threading model on
Android's bionic libc as well as for the driver.

## Device and environment

| | |
| --- | --- |
| Device | OPPO **CPH2399** (`ro.product.manufacturer` reports `OnePlus`), serial `<device serial redacted>` [redacted 2026-09-24 for the public repository; the original serial remains in git history — see ADR-0005] |
| Android | **16** (`ro.build.version.release`), API level **36** (`ro.build.version.sdk`), build `BP4A.251205.006` |
| ABI | `arm64-v8a` |
| Kernel | `Linux 4.19.191+ aarch64` |
| Host | Windows 11 Pro 26200, `rustc 1.98.1 (48a229cea 2026-09-01)`, `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| NDK | **r28c** `28.2.13676358` (already installed under `%LOCALAPPDATA%\Android\Sdk\ndk\`); NDK clang 19.0.1 |
| Min API built against | **26** — the same arbitrary, comfortably-modern level S6 chose; nothing in the dependency graph forced it |
| Database | Oracle Database 19c Enterprise Edition 19.0.0.0.0, in Docker on the host, bound to loopback only (`127.0.0.1:1521` TCP, `127.0.0.1:2484` TCPS, service `RELDEX`) |
| Transport | USB — `adb reverse tcp:1521` / `tcp:2484`, so the device's own `127.0.0.1` is forwarded over the cable to the host's loopback listener. Nothing was exposed on any network interface. |

Confirmed from inside the process, not assumed:
`std::env::consts::OS`/`ARCH` = **`android`/`aarch64`**, pointer width 64,
`argv[0]` = `/data/local/tmp/reldex-device-check/reldex-device-check`.

## Build

Cross-compiling on a **Windows** host needed **no extra tooling**: no
`cargo-ndk`, no `cmake`, no NASM, no `bindgen`/`libclang`, no Go. `aws-lc-sys`
0.45.0 compiled its C and assembly with the NDK's own clang. This matters
because S6's clean build was on a Linux CI host and the open question was
whether a Windows host would need more; it does not.

Three environment variables, exported for the build process only and
deliberately **not** written into a tracked `.cargo/config.toml` (where the NDK
lives is a property of the machine):

```text
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER
  = <NDK>/toolchains/llvm/prebuilt/windows-x86_64/bin/aarch64-linux-android26-clang.cmd
CC_aarch64_linux_android  = <same clang wrapper>
AR_aarch64_linux_android  = <NDK>/toolchains/llvm/prebuilt/windows-x86_64/bin/llvm-ar.exe

cargo build --release --target aarch64-linux-android \
  --manifest-path <repo>/Cargo.toml -p reldex-core-poc
```

Result — `file` confirms a genuine Android binary, not a generic Linux one:

```text
reldex-device-check: ELF 64-bit LSB pie executable, ARM aarch64, version 1 (SYSV),
dynamically linked, interpreter /system/bin/linker64, for Android 26,
built by NDK r28c (13676358), not stripped
```

| | bytes |
| --- | --- |
| `reldex-device-check`, release, unstripped | 5,132,496 |
| the same, `llvm-strip --strip-all` | 4,128,560 |

(`reldex-core-poc`, the existing desktop CLI, cross-compiles from the same
`cargo build` invocation: 4,810,624 B unstripped. It was the binary used for the
first on-device `ping`, before the purpose-built harness existed.)

## Exact commands, redacted

```bash
# 1. everything, through the runner (Git Bash on Windows)
tools/android-device/run-on-device.sh --show-commands

# which is, in essence:
adb push target/aarch64-linux-android/release/reldex-device-check \
         /data/local/tmp/reldex-device-check/
adb push tools/oracle-test-db/wallet/ewallet.pem \
         /data/local/tmp/reldex-device-check/wallet/
adb shell 'chmod 700 /data/local/tmp/reldex-device-check/reldex-device-check'
adb reverse tcp:1521 tcp:1521
adb reverse tcp:2484 tcp:2484

printf '@@<RELDEX_TEST_PWD>@@\n' | adb shell -T 'IFS= read -r RPW; RPW=${RPW#*@@}; RPW=${RPW%@@*};
  export RELDEX_TEST_ORACLE_PASSWORD="<redacted>"; unset RPW;
  export RELDEX_TEST_ORACLE_DSN='"'"'127.0.0.1:1521/RELDEX'"'"';
  export RELDEX_TEST_ORACLE_USER='"'"'RELDEX_TEST'"'"';
  export RELDEX_TEST_ORACLE_TCPS_DSN='"'"'tcps://localhost:2484/RELDEX'"'"';
  export RELDEX_TEST_ORACLE_TCPS_CA_DIR='"'"'/data/local/tmp/reldex-device-check/wallet'"'"';
  exec /data/local/tmp/reldex-device-check/reldex-device-check'
```

The password is read from the untracked `tools/oracle-test-db/.env` at run time
and handed to the device on `adb shell`'s **standard input**, so it is never on
a command line — not in the host's process list, not in the device's `ps` — and
never written to the device's filesystem. `adb shell -T` suppresses the pty, so
nothing echoes it back. Only the **public** CA certificate
(`wallet/ewallet.pem`, one `CERTIFICATE` block, no private key) was pushed; the
runner reads the PEM and refuses to push one containing a key.

## Results

All seven checks passed. Output below is verbatim from the 2026-09-20 run
through `run-on-device.sh`; earlier runs on the same day agreed within noise.

### (a) Connect and ping — the key evidence

```text
[PASS] ping
       session SessionId#1 on connection ConnectionId#1, cancel kind PreArmedDeadline
       ping 5.1ms, lifecycle usable
       server: Oracle Database 19c Enterprise Edition Release 19.0.0.0.0 - Production
```

Connect latency measured separately during bring-up: **282 ms** first connect,
**12 ms** ping (2026-09-19, `reldex-core-poc ping` on the device). `ping` across
later runs: 3.2 / 7.3 / 7.6 / 5.1 ms. The server banner is the database's own,
so the round trip is real.

### (b) Typed data

```text
[PASS] types
       NUMBER, DATE, TIMESTAMP and the server's TO_CHAR all matched exactly
       text round-tripped byte-exact: "ทดสอบภาษาไทย 🐘" (41 bytes, 14 characters)
```

Checked exactly, not approximately:

| value | asserted |
| --- | --- |
| `CAST(12345.6789 AS NUMBER(12,4))` | `12345.6789` |
| `CAST(-170141183460469231731687303715884105727 AS NUMBER)` | all 39 digits — no `f64` rounding |
| `DATE '2026-09-19'` | `2026-09-19T00:00:00` |
| `TIMESTAMP '2026-09-19 13:45:56.123456'` | `2026-09-19T13:45:56.123456000` |
| the server's own `TO_CHAR(…, 'YYYY-MM-DD"T"HH24:MI:SS.FF9')` | the same string — the driver's decoding is checked against the database, not against the test file |

Text was **written and read back**, not just embedded in a literal: the Thai
string spike S2 uses (combining marks above and below the base character) plus
a non-BMP character (`🐘` — one Rust `char`, two UTF-16 code units, four UTF-8
bytes: the value that breaks a driver sizing buffers in UCS-2), bound through
both `VARCHAR2(100 CHAR)` and `NVARCHAR2(100)`. Both came back byte-exact, and
the server's own `LENGTHB` (41) and `LENGTH` (14) matched Rust's view of the
same string — so a byte-level corruption that happened to survive the driver's
own decoder would still have failed.

### (c) Transaction

```text
[PASS] transaction
       after INSERT: has_possibly_active_transaction = true
       rollback: row is gone (COUNT(*) = 0)
       commit: the row is there, with its text intact
       table devchk_tx_22468_59426 dropped
```

Auto-commit off, as the architecture requires. Insert → rollback → the row is
gone; insert → commit → the row is there, with its Thai text intact. The table
name carries the pid and a timestamp, so a re-run never collides with a
leftover, and it is dropped at the end (a failed drop is reported, never
swallowed).

### (d) Large result

```text
[PASS] bulk
       100000 row(s), 2 column(s), in 6.6s = 15133 rows/s
       fetched 1000 rows at a time; checksum 977790
       peak RSS (VmHWM) 5500 kB before, 5956 kB after (+456 kB)
```

100 000 rows from `CONNECT BY LEVEL`, two columns (NUMBER and VARCHAR2),
fetched 1 000 at a time and **every value touched**, so the figure is a real
decode-and-read rate and not the cost of discarding rows unread.

Three runs: 15 717 / 15 358 / 15 133 rows/s (6.4–6.6 s). Peak RSS read by the
binary itself from `/proc/self/status` `VmHWM`: it grew by **0.45–0.63 MB**
across the whole 100 000-row fetch, from about 5.5 MB, ending under 6.2 MB —
the batch path does not accumulate the result.

**These numbers are not a benchmark of the device or the driver.** Every row
crosses a USB cable to a Docker-hosted database on the same laptop, which was
simultaneously serving another engineer's live test suite. Treat 15 k rows/s as
"the batched path works and is not pathological on ARM64", not as a throughput
figure to compare against anything.

### (e) TCPS

```text
[PASS] tcps
       dial tcps://localhost:2484/RELDEX
       server says USERENV.NETWORK_PROTOCOL = tcps, SERVICE_NAME = RELDEX
       certificate and host-name verification are on and cannot be turned off in this driver
       CA trusted from the pushed wallet directory (public certificate only)
```

A real TLS handshake, negotiated by `rustls`/`aws-lc-rs` compiled for ARM64
Android — the thing S6 could only *link*, never exercise.

Verification was **on**: this driver has no switch to turn certificate or
host-name checking off. The test CA was trusted from the pushed
`wallet/ewallet.pem`. The DSN uses `localhost`, not `127.0.0.1`, because the
listener's certificate carries the DNS name and no IP address in its
`subjectAltName` — spike S8 found this and has a test that depends on the
numeric form failing; `run-it.ps1` uses the same form and the runner mirrors
it. Through `adb reverse`, `localhost` on the device is the device's own
loopback, forwarded over USB.

The claim is the **server's**, not the client's: `sys_context('USERENV',
'NETWORK_PROTOCOL')` returned `tcps`. Anything short of asking the server would
be the client believing its own configuration. An ordinary query and Thai text
were then round-tripped over the encrypted transport.

### (f) Per-statement deadline

```text
[PASS] deadline
       armed 3.0s, stopped after 3.8s (overshoot 816.1ms)
       error: [Timeout] timeout: the call timeout armed for this statement expired
              session=NeedsValidation
       db-core lifecycle after the failure: needs-validation
       Timeout promises a recoverable session, and the session recovered
```

Spike S4's long statement (a three-way cartesian join over `all_objects`) with
a 3-second deadline armed. It returned `ErrorKind::Timeout` — a limit the
caller set, not a cancellation someone requested — with
`SessionState::NeedsValidation`, and the session then **actually survived**: a
probe query on it succeeded. The check fails if the driver reports a
recoverable session and the session does not recover, or reports `NetworkLost`
and it does.

Both runs where this check was armed correctly stopped between 3.8 s and 4.8 s
(overshoot 0.8–1.8 s), consistent with the deadline being `oracledb`'s socket
read timeout rather than a wall-clock limit on the operation as a whole.

**A bug this run found in the harness, worth recording.** The first attempt
armed the deadline and called `execute` only — and `execute` *returned
successfully*. This driver describes before it fetches (it asks `oracledb` for
zero prefetched rows so a select list it cannot decode safely is refused before
any value is decoded — the U-3 mitigation), so a query's work happens on the
**fetch**; stopping at `execute` times the describe, not the statement. Spike
S4's `run_to_first_batch` helper exists for exactly this reason. The check now
drives the first batch, and this is a property of the driver's design, not an
Android finding.

### (g) Device facts, from inside the process

```text
[PASS] facts
       std::env::consts::OS/ARCH = android/aarch64
       pointer width = 64 bits, pid = 22468
       argv[0] = /data/local/tmp/reldex-device-check/reldex-device-check
       VmHWM at start = 3608 kB
```

## What this does and does not prove

### It proves

- The Reldex Rust core and the pure-Rust Oracle thin driver **execute
  correctly on physical ARM64 Android hardware and a current Android OS**
  (Android 16 / API 36), built against API 26.
- `db-core`'s session model works there: a dedicated worker thread per session,
  commands over a channel, `Completion`-based waits, and a `close` that will not
  silently commit — all on bionic libc.
- A **real Oracle 19c connection** from the device: authentication, ping,
  describe, batched fetch, DDL, DML, commit and rollback.
- **Value fidelity** on ARM64: 39-digit NUMBER, DATE, TIMESTAMP(6), and
  Thai/non-BMP text through VARCHAR2 and NVARCHAR2, each cross-checked against
  the server's own rendering.
- A **real TLS handshake** from the device, with certificate and host-name
  verification on, confirmed by the server — `rustls`/`aws-lc-rs` works there,
  not merely links.
- The **deadline path** behaves as S4 documents, including the honesty
  property: the session state the driver reports is the state the session is
  actually in.
- Cross-compiling for Android from a **Windows** host needs no `cargo-ndk`,
  `cmake`, NASM, `bindgen` or Go.

### It does not prove

- **This is not an APK.** It is a native CLI binary pushed to
  `/data/local/tmp` and run through `adb shell`. There is no Qt, no QML, no
  JNI, no `Activity`, no Android permission model, no app sandbox, and no
  Google Play packaging or review. It runs as the **`shell`** user in the
  `shell` SELinux context — more permissive in some ways than an app's
  `untrusted_app` context, and differently restricted in others. Nothing here
  says the same code will run inside an app sandbox.
- **The network path is the USB cable.** `adb reverse` is a loopback forward
  over USB. No Wi-Fi, no cellular, no NAT, no captive portal, no VPN, no
  corporate proxy, no IPv6, no DNS resolution of a real hostname, no packet
  loss or latency resembling a mobile network. Android's per-app network
  restrictions (`INTERNET` permission, network-security config, Private DNS,
  data saver, VPN lockdown) were never in the path.
- **No process-lifecycle testing.** The process ran in the foreground for a few
  seconds. Nothing here exercises background/resume, Doze, App Standby, a
  socket killed while the screen is off, or reconnect after the OS suspends the
  app — `SPEC.md` §18's territory, and the thing a mobile OS does that a
  desktop never does. ("Stay awake" was on during the run.)
- **Not covered on-device at all:** LOB streaming, PL/SQL, `SAVEPOINT`,
  cancellation other than a pre-armed deadline, concurrency across sessions,
  metadata queries, reconnect/lost-session semantics. Those have desktop spike
  evidence (S5, S7, S9–S14) and no device evidence.
- **The performance figures are not benchmarks** — see (d).
- **One device, one OS version, one vendor.** CPH2399 on Android 16. Nothing
  about other chipsets, Android versions, or vendor network stacks.
- **Nothing about iOS.** No iOS device has run anything.

### What remains for the packaged-app path

In rough order:

1. Build the Rust core as a `cdylib`/`staticlib` for `arm64-v8a` and load it
   from an Android app — `mobile-link-check` already proves the link; the
   missing piece is the JNI (or Qt-for-Android) boundary and the FFI ADR that
   governs it, including what unwinding across it is allowed to do.
2. An actual APK with the Qt Quick UI, `<uses-permission INTERNET>`, and a
   network-security config (a cleartext database connection needs an explicit
   exemption on API 28+; a TCPS one still needs the private CA in a trust
   anchor or passed to the driver as here).
3. Run it over **real** Wi-Fi/cellular to a database reachable from that
   network — which raises the direct-connect question the owner has not yet
   decided (a mobile device reaching an on-prem or VPN-only database).
4. Background/resume and lost-session behaviour under Doze and App Standby.
5. The remaining spikes on-device: LOB, PL/SQL, cancellation, reconnect.
6. Repeat on at least one other chipset/Android version before calling mobile
   support anything but "validated on one device".

Until 1–4 are done, **`AGENTS.md`'s rule stands**: mobile direct-connect is not
supported. This run moves the Android side from "compiles" to "the core and
driver run on real hardware", which is a real step and not the whole of it.

## Criterion 7 after this run

`phase-0.md` criterion 7 asks for "physical-device evidence or a clearly
documented blocker" for Android **and** iOS.

- **Android:** physical-device evidence now exists (this document). The
  Android device checklist in `phase-0.md` gains: connect over TCP, connect
  over TCPS, SQL, transaction. Still unticked there: package a minimal native
  harness *as an app*, PL/SQL, cancellation beyond a deadline, LOB,
  background/resume, reconnect/lost-session.
- **iOS: unchanged.** No iOS device, no Mac-local Xcode run, no Apple Developer
  account has been used. The blocker documented in
  `phase-0-s6-mobile-cross-compile.md` stands in full.

So criterion 7 remains **met with limits, stated honestly** — the limits are
now smaller on the Android side and identical on the iOS side. It is not met
outright, and this document does not claim it is.

## Repeatability and housekeeping

- `tools/android-device/run-on-device.sh` (primary, Git Bash/POSIX) and
  `run-on-device.ps1` (PowerShell twin) build, push, open the tunnels, run the
  checks, print a redacted transcript and clean up. Both fail loudly with no
  device attached, with more than one attached, with a missing NDK or API
  level, or with no password in the env file.
- The phone was left exactly as found: pushed files removed from
  `/data/local/tmp`, no device setting touched, the USB-debugging authorisation
  untouched, and no `adb usb`/`tcpip`/`reboot`/`kill-server` ever run. The two
  `adb reverse` mappings are deliberately left in place at the owner's request
  (loopback-only, harmless, and gone on disconnect); `--remove-reverse` drops
  them.
- Every table the checks create is dropped before they finish.
- The database container was never stopped, recreated or rebound; it stayed on
  loopback throughout.
- CI needs no workflow change: `.github/workflows/mobile-cross-compile.yml`
  already cross-compiles `-p reldex-core-poc`, and `reldex-device-check` is a
  second binary in that same package, so it is built by the existing job.
- Host gates green in this worktree after the change: `cargo fmt --all --
  --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` (all suites ok, 0 failed).
