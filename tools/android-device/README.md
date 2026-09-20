# Android physical-device checks

Runs the Reldex Rust core and Oracle thin driver on a **physical ARM64 Android
phone** against the Phase 0 test database, and reports what held and what did
not.

This is the Android half of Phase 0 success criterion 7
(`docs/exec-plans/active/phase-0.md`). The recorded run, its results and an
honest account of what it does and does not prove are in
[`docs/exec-plans/active/phase-0-android-device.md`](../../docs/exec-plans/active/phase-0-android-device.md).
Read that before quoting any number from here.

## What runs

`reldex-device-check` (`crates/reldex-core-poc/src/bin/reldex-device-check.rs`),
cross-compiled for `aarch64-linux-android`, `adb push`ed to
`/data/local/tmp/reldex-device-check/` and run over `adb shell`. It goes
through `reldex-db-core`, so every database call happens on a session's
dedicated worker thread, not on the thread that asked for it.

| check | what it asserts |
| --- | --- |
| `facts` | `std::env::consts::OS`/`ARCH` are `android`/`aarch64`; prints pid, pointer width and `VmHWM` |
| `ping` | connect and ping through `db-core`; prints the server banner and the session's cancel kind |
| `types` | NUMBER (incl. 39 digits), DATE, TIMESTAMP(6) and Thai + non-BMP text through VARCHAR2 **and** NVARCHAR2, each checked against the server's own `TO_CHAR`/`LENGTHB`/`LENGTH` |
| `transaction` | insert → rollback → gone; insert → commit → there; unique table, dropped at the end |
| `bulk` | 100 000 rows in 1 000-row batches, every value touched; rows/s and peak RSS |
| `tcps` | TLS on 2484 with certificate **and** host-name verification on, confirmed by the server's `sys_context('USERENV','NETWORK_PROTOCOL')` |
| `deadline` | a per-statement deadline stops a long statement with a timeout-class error, and the session state the driver reports is the state the session is actually in |

## Prerequisites

- A physical arm64 Android device with USB debugging authorised
  (`adb devices` must list exactly one as `device`).
- `adb` on `PATH` (`%LOCALAPPDATA%\Android\Sdk\platform-tools`).
- An Android NDK under `%LOCALAPPDATA%\Android\Sdk\ndk\` — the script picks the
  newest installed one. **No `cargo-ndk`, `cmake`, NASM or `bindgen` is
  needed**; the script points cargo's linker/`CC`/`AR` at the NDK's own
  clang and `llvm-ar` through environment variables set for that process only.
- `rustup target add aarch64-linux-android`.
- The Phase 0 test database running locally and bound to loopback
  (`tools/oracle-test-db/`), with `tools/oracle-test-db/.env` filled in. For
  the `tcps` check, the TLS listener enabled and
  `tools/oracle-test-db/wallet/ewallet.pem` exported.

## Running it

`run-on-device.sh` is the entry point — Git Bash on Windows, or an ordinary
shell on Linux/macOS:

```bash
tools/android-device/run-on-device.sh
tools/android-device/run-on-device.sh --checks ping,tcps
tools/android-device/run-on-device.sh --skip-build --show-commands
```

| option | effect |
| --- | --- |
| `--checks a,b` | run only these checks, in the order given (default: all) |
| `--env-file PATH` | where to read the database password from (default: this checkout's `tools/oracle-test-db/.env`, else the main checkout's) |
| `--api-level N` | Android API level to build against (default 26, matching spike S6) |
| `--skip-build` | reuse the binary already in `target/` |
| `--keep-files` | leave the pushed files on the device |
| `--remove-reverse` | also remove the `adb reverse` mappings this run opened |
| `--show-commands` | print the exact device command, with the password redacted |

It fails loudly when no device is attached, when more than one is, when the
NDK or the API level is missing, or when the env file has no password.
The exit code is the check suite's: non-zero if anything failed.

`run-on-device.ps1` is a PowerShell twin with the same steps and the same
switches in PowerShell spelling (`-Checks`, `-EnvFile`, `-ApiLevel`,
`-SkipBuild`, `-KeepFiles`, `-RemoveReverse`, `-ShowCommands`). Keep the two in
step when changing either.

### Two shell gotchas

- **Git Bash rewrites device paths.** `adb shell ls /data/local/tmp` reaches
  the device as `ls C:/Program Files/Git/data/local/tmp`. Every adb call in the
  script goes through a wrapper that sets `MSYS_NO_PATHCONV=1`; do the same
  (or write `//data/local/tmp`) for any ad-hoc adb command you type yourself.
  Windows paths handed to `adb`/`cargo` go through `cygpath -m` for the same
  reason.
- **Do not redirect stderr on Windows PowerShell 5.1.** `… | Tee-Object` with
  `2>&1` turns every `cargo` progress line into a `NativeCommandError`. Use
  `Start-Transcript`, or the bash script with `tee`. The `.ps1` defends itself
  around the two native calls that matter, but the habit is still worth
  avoiding.

## How the phone reaches the database

The test database is bound to the PC's loopback only (`127.0.0.1:1521` and
`127.0.0.1:2484`) and stays that way. The script opens
`adb reverse tcp:1521 tcp:1521` and `tcp:2484`, so a connection to
`127.0.0.1:1521` **on the device** is forwarded over the USB cable to the PC's
loopback listener. Nothing is exposed on any network interface.

The TCPS DSN is `tcps://localhost:2484/RELDEX`, not the numeric form: the test
listener's certificate carries the DNS name and no IP address, and host-name
verification is on and cannot be turned off in this driver. Spike S8 found
that; `tools/oracle-test-db/run-it.ps1` uses the same form.

**The reverse mappings are left in place** when the run ends. They are
loopback-only, harmless, and vanish when the device disconnects, and leaving
them makes the next run cheaper. Pass `--remove-reverse` to drop them.

## Secrets

- The password is read from the untracked `.env` at run time and handed to the
  device process over `adb shell`'s **standard input**, so it is never on a
  command line — not in this PC's process list, not in the device's `ps` — and
  never written to the device's filesystem.
- `adb shell -T` is used so no pty is allocated and nothing echoes it back.
- `--show-commands` prints the command with the value redacted. Nothing in this
  directory prints a credential; the bash script runs `set +x` and neither
  script sources the env file (it is parsed as data, so a stray command in it
  cannot run). Do not add tracing (`set -x`, `Set-PSDebug -Trace`) around
  either, and do not `env`-dump the device process.
- Only the **public** CA certificate is pushed. The script reads the PEM first
  and refuses to push it if it contains a private key.
- Two quirks worth knowing if you change the stdin path: Windows PowerShell
  prepends a UTF-8 BOM to a native command's stdin and appends CR. The value is
  therefore wrapped in `@@` sentinels and the device shell strips to them,
  which removes both without touching the password.

## What the script changes on the phone

Nothing but its own files. It does not touch developer options, the USB mode,
the "stay awake" setting or the USB-debugging authorisation, and it never runs
`adb usb`, `adb tcpip`, `adb disable-verity`, `adb reboot` or
`adb kill-server`. Cleanup removes `/data/local/tmp/reldex-device-check`
(unless `--keep-files`); the checks drop every database table they create.

## Not covered here

This is a native CLI binary run from `/data/local/tmp` through `adb shell`. It
is **not** an APK: no Qt, no JNI, no Android permission model, and it runs in
the `shell` user's SELinux context, not an app sandbox. The network path is the
USB cable, not Wi-Fi or cellular. See the evidence document's "What this does
and does not prove" section before treating any of this as app-level support.
