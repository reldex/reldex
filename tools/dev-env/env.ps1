# Reldex Phase 1 toolchain environment (PowerShell)
# Sets PATH / CMAKE_PREFIX_PATH and imports the MSVC (vcvars64) environment
# into the CURRENT PROCESS ONLY. Does not touch user/machine PATH or registry.
#
# Usage:  . .\env.ps1        (dot-source it so the env vars persist in your shell)

$ErrorActionPreference = "Stop"

# --- Qt ---------------------------------------------------------------
$env:QT_DIR = "C:\Qt\6.8.3\msvc2022_64"
$env:CMAKE_PREFIX_PATH = $env:QT_DIR

# --- CMake / Ninja (portable, per-user, under C:\Qt\Tools) -------------
$cmakeBin = "C:\Qt\Tools\CMake\bin"
$ninjaBin = "C:\Qt\Tools\Ninja"

# --- MSVC (vcvars64) - import into THIS process only -------------------
$vcvarsall = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvarsall)) {
    throw "vcvars64.bat not found at $vcvarsall - check the Visual Studio installation path."
}

# vcvars64.bat internally shells out to vswhere.exe by bare name for extra
# SDK/toolset detection; make sure it can be found (process-local PATH only)
# to avoid a harmless-but-noisy "'vswhere.exe' is not recognized" warning.
$vswhereDir = "C:\Program Files (x86)\Microsoft Visual Studio\Installer"
$vcvarsOutput = cmd /c "set `"PATH=$vswhereDir;%PATH%`" && `"$vcvarsall`" && set" 2>$null
foreach ($line in $vcvarsOutput) {
    if ($line -match "^([^=]+)=(.*)$") {
        Set-Item -Path "Env:\$($matches[1])" -Value $matches[2]
    }
}

# --- Prepend Qt/CMake/Ninja to PATH (process-local only) ---------------
$env:PATH = "$($env:QT_DIR)\bin;$cmakeBin;$ninjaBin;$($env:PATH)"

Write-Host "Reldex toolchain environment ready (this process only):"
Write-Host "  QT_DIR             = $($env:QT_DIR)"
Write-Host "  CMAKE_PREFIX_PATH  = $($env:CMAKE_PREFIX_PATH)"
Write-Host "  cmake              = $(where.exe cmake 2>$null | Select-Object -First 1)"
Write-Host "  ninja              = $(where.exe ninja 2>$null | Select-Object -First 1)"
Write-Host "  cl (MSVC)          = $(where.exe cl 2>$null | Select-Object -First 1)"

# vcvars64.bat / its internal helper calls can leave a stale non-zero
# $LASTEXITCODE behind even though environment setup succeeded; reset it
# so it doesn't confuse callers checking $LASTEXITCODE right after sourcing.
$global:LASTEXITCODE = 0
