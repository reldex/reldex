# Runs the opt-in Oracle integration tests against the local test database.
#
#   pwsh tools/oracle-test-db/run-it.ps1                 # every spike
#   pwsh tools/oracle-test-db/run-it.ps1 s2_fidelity     # one test file
#   pwsh tools/oracle-test-db/run-it.ps1 s4_cancel -- --test-threads=1
#   $env:RELDEX_IT_PACKAGE = 'reldex-core-poc'; pwsh tools/oracle-test-db/run-it.ps1 m5_2_result_store_live
#   $env:QT_QPA_PLATFORM = 'offscreen'; $env:RELDEX_IT_EXEC = 'build\ui-RelWithDebInfo\tst_coreinfo.exe'
#       pwsh tools/oracle-test-db/run-it.ps1 connectFlowAgainstTheRealDatabase
#
# `RELDEX_IT_PACKAGE` picks the crate whose `oracle-it` tests run; the default
# is the Oracle driver's. Tests that need `db-core` as well as the driver live
# in `reldex-core-poc` (a driver crate may not depend on `db-core`). The bash
# twin, `run-it.sh`, is the primary entry point and reads the same variables.
#
# `RELDEX_IT_EXEC` runs that program instead of `cargo test`, with the same
# environment and the arguments as given (M3.3: the UI's live end-to-end test,
# a Qt test binary; Qt's DLLs must be on PATH). A relative path is taken from
# the repository root.
#
# Run S4 single-threaded, as above: its long joins, `KILL SESSION` and 20-second
# sleep load this single-instance container enough to flip U-6's outcome when
# they run in parallel. Works under both `pwsh` and Windows PowerShell 5.1
# (`powershell`).
#
# It loads `tools/oracle-test-db/.env` (untracked; see `.env.example`) and turns
# it into the environment the tests read. The passwords are never echoed, never
# passed on a command line, and never written to a file: they go straight into
# this process's environment, which the child `cargo` inherits.
#
# The tests are behind the `oracle-it` feature, so `cargo test --workspace`
# stays green on a machine with no database.

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Test,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $here '..\..')
$envFile = Join-Path $here '.env'

if (-not (Test-Path $envFile)) {
    Write-Error "$envFile not found. Copy .env.example to .env and fill it in (see README.md)."
}

$values = @{}
foreach ($line in Get-Content $envFile) {
    $trimmed = $line.Trim()
    if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
    $split = $trimmed.IndexOf('=')
    if ($split -lt 1) { continue }
    $key = $trimmed.Substring(0, $split).Trim()
    $value = $trimmed.Substring($split + 1).Trim().Trim('"').Trim("'")
    $values[$key] = $value
}

foreach ($required in @('ORACLE_PWD', 'RELDEX_TEST_PWD')) {
    if (-not $values.ContainsKey($required) -or $values[$required] -eq '') {
        Write-Error "$required is missing from $envFile"
    }
}

# The listener answers to the SERVICE NAME, not the SID: the `//host:port:SID`
# shorthand does not work against this container (see README.md).
$env:RELDEX_TEST_ORACLE_DSN = '127.0.0.1:1521/RELDEX'
$env:RELDEX_TEST_ORACLE_USER = 'RELDEX_TEST'
$env:RELDEX_TEST_ORACLE_PASSWORD = $values['RELDEX_TEST_PWD']

# Opt-in extra, used only by spike S4's privileged-cancel candidate
# (`ALTER SYSTEM CANCEL SQL`). Tests that need it skip themselves when it is
# absent, so an ordinary run never requires a DBA password.
$env:RELDEX_TEST_ORACLE_SYSTEM_USER = 'SYSTEM'
$env:RELDEX_TEST_ORACLE_SYSTEM_PASSWORD = $values['ORACLE_PWD']

# Opt-in extra, used only by spike S13 (`AS SYSDBA` over the listener, which
# this image authenticates against its password file). Same password as above;
# a separate pair of variables so the role is explicit at the call site and a
# checkout that does not want a SYSDBA test can clear just these two.
$env:RELDEX_TEST_ORACLE_SYSDBA_USER = 'SYS'
$env:RELDEX_TEST_ORACLE_SYSDBA_PASSWORD = $values['ORACLE_PWD']

# Spike S8 (TCPS). Set only when the TLS listener has been enabled and the CA
# certificate exported (`startup/10_enable_tcps.sh`, then the export step in
# README.md, "TCPS"); the S8 tests skip themselves and say so otherwise.
#
# `localhost`, not `127.0.0.1`: the listener's certificate carries the DNS name
# and no IP address, and the client verifies whatever the descriptor's HOST
# says. That is not a quirk of this setup - it is what S8 found, and
# `s8_tcps.rs` has a test that depends on the numeric form failing.
$walletDir = Join-Path $here 'wallet'
$untrustedDir = Join-Path $here 'wallet-untrusted'
if (Test-Path (Join-Path $walletDir 'ewallet.pem')) {
    $env:RELDEX_TEST_ORACLE_TCPS_DSN = 'tcps://localhost:2484/RELDEX'
    $env:RELDEX_TEST_ORACLE_TCPS_CA_DIR = $walletDir
    if (Test-Path (Join-Path $untrustedDir 'ewallet.pem')) {
        $env:RELDEX_TEST_ORACLE_TCPS_WRONG_CA_DIR = $untrustedDir
    }
    Write-Host "tcps:     $env:RELDEX_TEST_ORACLE_TCPS_DSN (CA from $env:RELDEX_TEST_ORACLE_TCPS_CA_DIR)"
}

Write-Host "database: $env:RELDEX_TEST_ORACLE_USER@$env:RELDEX_TEST_ORACLE_DSN"

if ($env:RELDEX_IT_EXEC) {
    Write-Host "program:  $env:RELDEX_IT_EXEC"
    $program = @()
    if ($Test) { $program += $Test }
    $program += @($Rest | Where-Object { $_ -ne '--' })
    Push-Location $repo
    try {
        & $env:RELDEX_IT_EXEC @program
        $code = $LASTEXITCODE
    }
    finally {
        Pop-Location
        Remove-Item Env:\RELDEX_TEST_ORACLE_PASSWORD -ErrorAction SilentlyContinue
        Remove-Item Env:\RELDEX_TEST_ORACLE_SYSTEM_PASSWORD -ErrorAction SilentlyContinue
        Remove-Item Env:\RELDEX_TEST_ORACLE_SYSDBA_PASSWORD -ErrorAction SilentlyContinue
    }
    exit $code
}

$package = if ($env:RELDEX_IT_PACKAGE) { $env:RELDEX_IT_PACKAGE } else { 'reldex-driver-oracle-thin' }
Write-Host "package:  $package"

$cargo = @('test', '-p', $package, '--features', 'oracle-it')
# A leading `--` means "no test file was named; pass the rest to the harness".
if ($Test -and $Test -ne '--') {
    $cargo += @('--test', $Test)
}
# Everything else goes to the test harness, behind the `--` separator that
# cargo requires. The separator is inserted here rather than taken from the
# command line because Windows PowerShell 5.1 consumes a bare `--` before the
# script is entered (pwsh 7 passes it through), which would otherwise make the
# documented `run-it.ps1 s4_cancel -- --test-threads=1` fail with
# "unexpected argument '--test-threads' found".
$harness = @($Rest | Where-Object { $_ -ne '--' })
if ($harness.Count -gt 0) {
    $cargo += '--'
    $cargo += $harness
}

Push-Location $repo
try {
    & cargo @cargo
    $code = $LASTEXITCODE
}
finally {
    Pop-Location
    # Do not leave credentials in the shell that invoked this script.
    Remove-Item Env:\RELDEX_TEST_ORACLE_PASSWORD -ErrorAction SilentlyContinue
    Remove-Item Env:\RELDEX_TEST_ORACLE_SYSTEM_PASSWORD -ErrorAction SilentlyContinue
    Remove-Item Env:\RELDEX_TEST_ORACLE_SYSDBA_PASSWORD -ErrorAction SilentlyContinue
}

exit $code
