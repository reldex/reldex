# Runs the Reldex physical-device checks on an attached Android phone.
#
#   pwsh tools/android-device/run-on-device.ps1
#   pwsh tools/android-device/run-on-device.ps1 -Checks ping,tcps
#   pwsh tools/android-device/run-on-device.ps1 -SkipBuild -RemoveReverse
#
# It cross-compiles `reldex-device-check` for `aarch64-linux-android`, pushes it
# (and, for the TCPS check, the test CA certificate) to /data/local/tmp, opens
# the `adb reverse` tunnels the device needs to reach this PC's loopback-only
# test database, runs the checks with the credentials in the device process's
# environment, and then removes the files it pushed.
#
# It changes nothing on the phone: no developer-options or USB setting, no
# `adb usb`/`adb tcpip`/`adb reboot`/`adb kill-server`, and it never touches
# the USB-debugging authorisation. The `adb reverse` mappings are left in place
# for the next run unless `-RemoveReverse` is passed.
#
# Works under both `pwsh` 7 and Windows PowerShell 5.1 (`powershell`).
#
# --- Secrets ---------------------------------------------------------------
# The password is read from `tools/oracle-test-db/.env` (untracked) at run time
# and handed to the device over the `adb shell` **standard input**, so it never
# appears on a command line (not in this PC's process list, not in the device's
# `ps`), never lands on the device's filesystem, and never reaches this script's
# own output. `-WhatIfCommands` prints the commands for the record with the
# secret redacted. Nothing here echoes a value, and `Set-PSDebug`/tracing must
# not be turned on around it.
#
# Only the **public** CA certificate (`wallet/ewallet.pem`, a single
# CERTIFICATE block) is pushed; the script refuses to push a file containing a
# private key.

[CmdletBinding()]
param(
    # Which checks to run, in order. Empty means all of them.
    [ValidateSet('facts', 'ping', 'types', 'transaction', 'bulk', 'tcps', 'deadline')]
    [string[]]$Checks = @(),

    # Path to the untracked env file holding the test database's passwords.
    # Empty means: this checkout's own copy when it has one, otherwise the main
    # checkout's — a linked worktree does not carry the untracked .env.
    [string]$EnvFile = '',

    # Android API level to build against. 26 matches spike S6's CI job.
    [int]$ApiLevel = 26,

    # Reuse the binary already in target/ instead of building.
    [switch]$SkipBuild,

    # Leave the pushed files on the device instead of removing them.
    [switch]$KeepFiles,

    # Also remove the `adb reverse` mappings this run opened. They are left in
    # place by default: they are loopback-only, harmless, disappear when the
    # device disconnects, and the owner wants them usable for the next run.
    [switch]$RemoveReverse,

    # Print the exact commands (redacted) instead of only running them.
    [switch]$ShowCommands
)

$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$remoteDir = '/data/local/tmp/reldex-device-check'
$target = 'aarch64-linux-android'
$binaryName = 'reldex-device-check'

function Write-Step([string]$text) { Write-Host "==> $text" }

# --- 1. Device -------------------------------------------------------------

$adb = (Get-Command adb -ErrorAction SilentlyContinue)
if (-not $adb) {
    throw "adb is not on PATH. Add %LOCALAPPDATA%\Android\Sdk\platform-tools."
}
$adb = $adb.Source

$devices = @(& $adb devices | Select-Object -Skip 1 | Where-Object { $_ -match '\sdevice$' })
if ($devices.Count -eq 0) {
    throw "No Android device is attached (or USB debugging is not authorised). 'adb devices' must list one as 'device'."
}
if ($devices.Count -gt 1) {
    throw "More than one device is attached; this script does not choose for you. Detach all but one."
}
Write-Step "Device facts"
$props = @(
    'ro.product.model', 'ro.product.manufacturer', 'ro.build.version.release',
    'ro.build.version.sdk', 'ro.product.cpu.abi', 'ro.build.id'
)
foreach ($p in $props) {
    $v = (& $adb shell getprop $p).Trim()
    Write-Host ("    {0,-28} {1}" -f $p, $v)
}
Write-Host ("    {0,-28} {1}" -f 'kernel', (& $adb shell uname -srm).Trim())

# --- 2. Build --------------------------------------------------------------

$binary = Join-Path $repo "target\$target\release\$binaryName"

if (-not $SkipBuild) {
    $sdk = Join-Path $env:LOCALAPPDATA 'Android\Sdk'
    $ndkRoot = Join-Path $sdk 'ndk'
    if (-not (Test-Path $ndkRoot)) { throw "No Android NDK under $ndkRoot." }
    # Newest installed NDK, so a later upgrade needs no edit here.
    $ndk = (Get-ChildItem $ndkRoot -Directory | Sort-Object Name -Descending | Select-Object -First 1).FullName
    $ndkBin = Join-Path $ndk 'toolchains\llvm\prebuilt\windows-x86_64\bin'
    $clang = Join-Path $ndkBin "aarch64-linux-android$ApiLevel-clang.cmd"
    $llvmAr = Join-Path $ndkBin 'llvm-ar.exe'
    if (-not (Test-Path $clang)) { throw "$clang not found; API level $ApiLevel is not in this NDK." }

    Write-Step "Building $binaryName for $target (API $ApiLevel) with NDK $(Split-Path $ndk -Leaf)"
    # Set only for this process, never in a tracked .cargo/config.toml: the
    # linker/CC/AR a checkout needs depends on where its NDK is installed.
    $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $clang
    $env:CC_aarch64_linux_android = $clang
    $env:AR_aarch64_linux_android = $llvmAr
    # cargo writes its progress to stderr. Under Windows PowerShell 5.1 a
    # caller who piped this script through `2>&1` would otherwise see each of
    # those lines turned into a terminating NativeCommandError; the exit code
    # below is the real verdict, so the preference is relaxed for this call.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & cargo build --release --target $target --manifest-path (Join-Path $repo 'Cargo.toml') -p reldex-core-poc
    $built = $LASTEXITCODE
    $ErrorActionPreference = $previous
    if ($built -ne 0) { throw "cargo build failed with exit code $built." }
}
if (-not (Test-Path $binary)) { throw "$binary not found; run without -SkipBuild." }
Write-Host ("    binary {0} bytes" -f (Get-Item $binary).Length)

# --- 3. Credentials --------------------------------------------------------

if ($EnvFile -eq '') {
    $local = Join-Path $repo 'tools\oracle-test-db\.env'
    $EnvFile = if (Test-Path $local) { $local } else { Join-Path $repo '..\..\tools\oracle-test-db\.env' }
}
$EnvFile = (Resolve-Path $EnvFile -ErrorAction SilentlyContinue)
if (-not $EnvFile) {
    throw "The env file was not found. Pass -EnvFile <path> (default: the main checkout's tools/oracle-test-db/.env)."
}
$values = @{}
foreach ($line in Get-Content $EnvFile) {
    $trimmed = $line.Trim()
    if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
    $split = $trimmed.IndexOf('=')
    if ($split -lt 1) { continue }
    $values[$trimmed.Substring(0, $split).Trim()] = $trimmed.Substring($split + 1).Trim().Trim('"').Trim("'")
}
if (-not $values.ContainsKey('RELDEX_TEST_PWD') -or $values['RELDEX_TEST_PWD'] -eq '') {
    throw "RELDEX_TEST_PWD is missing from $EnvFile"
}
$secret = $values['RELDEX_TEST_PWD']

# The device reaches this PC's loopback-only listeners through `adb reverse`,
# so from the device's point of view the database is on its own 127.0.0.1.
$dsn = '127.0.0.1:1521/RELDEX'
$user = 'RELDEX_TEST'
# `localhost`, not `127.0.0.1`: the test listener's certificate carries the DNS
# name and no IP address, and host-name verification is on and cannot be turned
# off in this driver. Spike S8 found this; `run-it.ps1` uses the same form.
$tcpsDsn = 'tcps://localhost:2484/RELDEX'

# --- 4. Wallet (public CA certificate only) --------------------------------

$walletSource = Join-Path (Split-Path $EnvFile -Parent) 'wallet\ewallet.pem'
$pushWallet = $false
if (Test-Path $walletSource) {
    $pem = Get-Content $walletSource -Raw
    if ($pem -match '-----BEGIN [A-Z ]*PRIVATE KEY-----') {
        throw "$walletSource contains a private key; refusing to push it to the device."
    }
    if ($pem -notmatch '-----BEGIN CERTIFICATE-----') {
        throw "$walletSource holds no certificate."
    }
    $pushWallet = $true
}
else {
    Write-Host "    note: $walletSource not found; the tcps check will skip."
}

# --- 5. Push + tunnels -----------------------------------------------------

$reverses = @()
$pushed = $false
try {
    Write-Step "Pushing to $remoteDir"
    & $adb shell "rm -rf $remoteDir; mkdir -p $remoteDir/wallet" | Out-Null
    $pushed = $true
    & $adb push $binary "$remoteDir/" | Out-Null
    & $adb shell "chmod 700 $remoteDir/$binaryName" | Out-Null
    if ($pushWallet) {
        & $adb push $walletSource "$remoteDir/wallet/" | Out-Null
        & $adb shell "chmod 600 $remoteDir/wallet/ewallet.pem" | Out-Null
    }

    Write-Step "Opening adb reverse tunnels (device loopback -> this PC's loopback)"
    foreach ($port in 1521, 2484) {
        & $adb reverse "tcp:$port" "tcp:$port" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "adb reverse tcp:$port failed." }
        $reverses += $port
        Write-Host "    tcp:$port -> tcp:$port"
    }

    # --- 6. Run ------------------------------------------------------------

    # The password arrives on stdin. Two things make that robust:
    #  * the `@@` sentinels, because Windows PowerShell prepends a UTF-8 BOM to
    #    a native command's stdin and appends CR; stripping to the sentinels
    #    removes both without touching the value;
    #  * `read -r`, so a backslash in the password is not an escape.
    $exports = @(
        "export RELDEX_TEST_ORACLE_DSN='$dsn'",
        "export RELDEX_TEST_ORACLE_USER='$user'"
    )
    if ($pushWallet) {
        $exports += "export RELDEX_TEST_ORACLE_TCPS_DSN='$tcpsDsn'"
        $exports += "export RELDEX_TEST_ORACLE_TCPS_CA_DIR='$remoteDir/wallet'"
    }
    $prelude = 'IFS= read -r RPW; RPW=${RPW#*@@}; RPW=${RPW%@@*}; ' +
               'export RELDEX_TEST_ORACLE_PASSWORD="$RPW"; unset RPW; ' +
               ($exports -join '; ') + '; '
    $argument = if ($Checks.Count -gt 0) { ' ' + ($Checks -join ' ') } else { '' }
    $remoteCommand = $prelude + "exec $remoteDir/$binaryName$argument"

    if ($ShowCommands) {
        Write-Step "Commands, for the record (password redacted)"
        Write-Host "    printf '@@<RELDEX_TEST_PWD>@@\n' | adb shell -T `"$($remoteCommand -replace '\$RPW', '<redacted>')`""
    }

    Write-Step "Running $binaryName on the device"
    # `-T` keeps adb from allocating a pty, so nothing echoes the secret back.
    # Same stderr caveat as the cargo call above: the exit code is the verdict.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    ('@@' + $secret + '@@') | & $adb shell -T $remoteCommand
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
}
finally {
    $secret = $null
    Write-Step "Cleaning up"
    # Only this run's own artefacts. Nothing here touches a device setting, the
    # USB-debugging authorisation, or the adb server.
    if (-not $KeepFiles) {
        if ($pushed) {
            & $adb shell "rm -rf $remoteDir" | Out-Null
            Write-Host "    removed $remoteDir"
        }
    }
    else {
        Write-Host "    -KeepFiles: $remoteDir is still on the device."
    }
    if ($RemoveReverse) {
        foreach ($port in $reverses) { & $adb reverse --remove "tcp:$port" | Out-Null }
        Write-Host "    removed $($reverses.Count) reverse tunnel(s)"
    }
    elseif ($reverses.Count -gt 0) {
        Write-Host "    kept $($reverses.Count) reverse tunnel(s); pass -RemoveReverse to drop them"
    }
}

if ($code -ne 0) {
    Write-Error "reldex-device-check reported failures (exit code $code)."
}
exit $code
