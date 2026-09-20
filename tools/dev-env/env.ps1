# Reldex Phase 1 toolchain environment (PowerShell)
# Sets PATH / CMAKE_PREFIX_PATH and imports the MSVC (vcvars64) environment
# into the CURRENT PROCESS ONLY. Does not touch user/machine PATH or registry.
#
# Usage:  . .\env.ps1        (dot-source it so the env vars persist in your shell)

$ErrorActionPreference = "Stop"

# --- Qt (only this developer's fixed local install path) --------------
# A CI runner (M1.4, ui.yml) installs Qt itself via jurplel/install-qt-action,
# which sets its own Qt6_DIR/PATH -- so QT_DIR/CMAKE_PREFIX_PATH are only set
# here when this exact local install exists, rather than overriding whatever
# is already there.
$qtDirCandidate = "C:\Qt\6.8.3\msvc2022_64"
if (Test-Path $qtDirCandidate) {
    $env:QT_DIR = $qtDirCandidate
    $env:CMAKE_PREFIX_PATH = $env:QT_DIR
} else {
    Write-Host "env.ps1: no local Qt install at $qtDirCandidate -- leaving QT_DIR/CMAKE_PREFIX_PATH as already set (e.g. by CI)."
}

# --- CMake / Ninja (portable, per-user, under C:\Qt\Tools) -------------
# Same reasoning: only prepended when they exist. A CI runner ships its own
# cmake/ninja already on PATH.
$cmakeBin = "C:\Qt\Tools\CMake\bin"
$ninjaBin = "C:\Qt\Tools\Ninja"

# --- MSVC (vcvars64) - import into THIS process only -------------------
# Found via vswhere rather than a hardcoded edition path (the previous
# version of this script hardcoded "...\2022\Community\...", which only
# happened to match this developer's workstation. Discovered as a real
# portability bug in M1.4 (ui.yml): GitHub-hosted windows-latest runners
# ship Visual Studio *Enterprise* 2022 at a different path, so the
# hardcoded form failed there outright). This is the same tool
# docs/exec-plans/active/phase-1-toolchain.md section 2 step 7 already uses
# by hand to verify the toolchain.
$vswhereDir = "C:\Program Files (x86)\Microsoft Visual Studio\Installer"
$vswhere = Join-Path $vswhereDir "vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere.exe not found at $vswhere - is Visual Studio installed?"
}
$vsInstallPath = & $vswhere -latest -products '*' `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath
if (-not $vsInstallPath) {
    throw "vswhere found no Visual Studio install with the VC++ x86/x64 toolset."
}
$vcvarsall = Join-Path $vsInstallPath "VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvarsall)) {
    throw "vcvars64.bat not found at $vcvarsall."
}

# vcvars64.bat internally shells out to vswhere.exe by bare name for extra
# SDK/toolset detection; make sure it can be found (process-local PATH only)
# to avoid a harmless-but-noisy "'vswhere.exe' is not recognized" warning.
$vcvarsOutput = cmd /c "set `"PATH=$vswhereDir;%PATH%`" && `"$vcvarsall`" && set" 2>$null
foreach ($line in $vcvarsOutput) {
    if ($line -match "^([^=]+)=(.*)$") {
        Set-Item -Path "Env:\$($matches[1])" -Value $matches[2]
    }
}

# --- Prepend Qt/CMake/Ninja to PATH (process-local only, only what exists) ---
$extraPath = ""
if (Test-Path $qtDirCandidate) { $extraPath = "$qtDirCandidate\bin" }
if (Test-Path $cmakeBin) { $extraPath = if ($extraPath) { "$extraPath;$cmakeBin" } else { $cmakeBin } }
if (Test-Path $ninjaBin) { $extraPath = if ($extraPath) { "$extraPath;$ninjaBin" } else { $ninjaBin } }
if ($extraPath) { $env:PATH = "$extraPath;$($env:PATH)" }

Write-Host "Reldex toolchain environment ready (this process only):"
Write-Host "  QT_DIR             = $(if ($env:QT_DIR) { $env:QT_DIR } else { '<not set by this script -- using whatever was already set>' })"
Write-Host "  CMAKE_PREFIX_PATH  = $(if ($env:CMAKE_PREFIX_PATH) { $env:CMAKE_PREFIX_PATH } else { '<not set by this script -- using whatever was already set>' })"
Write-Host "  cmake              = $(where.exe cmake 2>$null | Select-Object -First 1)"
Write-Host "  ninja              = $(where.exe ninja 2>$null | Select-Object -First 1)"
Write-Host "  cl (MSVC)          = $(where.exe cl 2>$null | Select-Object -First 1)"

# vcvars64.bat / its internal helper calls can leave a stale non-zero
# $LASTEXITCODE behind even though environment setup succeeded; reset it
# so it doesn't confuse callers checking $LASTEXITCODE right after sourcing.
$global:LASTEXITCODE = 0
