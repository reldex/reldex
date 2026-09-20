#!/usr/bin/env bash
# Reldex Phase 1 toolchain environment (Git Bash / POSIX sh) - PRIMARY entry
# point for this project. Sets PATH / CMAKE_PREFIX_PATH and imports the MSVC
# (vcvars64) environment into the CURRENT SHELL PROCESS ONLY. Does not touch
# user/machine PATH or registry.
#
# Usage:  source ./env.sh
#
# NOTE on ordering: the MSVC import below MUST run before we export any of
# our own QT_*/CMAKE_* variables. The import works by spawning a cmd.exe
# child (to run vcvars64.bat) which inherits bash's *entire* environment,
# including anything we've already exported; MSYS silently rewrites
# POSIX-style values (e.g. "/c/Qt/...") to Windows form ("C:/Qt/...") the
# moment they cross into that child process. cmd's "set" dump then contains
# those Windows-form values, and re-importing it would clobber our own
# POSIX-style exports right back to Windows form. Exporting our own
# variables only *after* the import sidesteps this entirely.

# --- CMake / Ninja (portable, per-user, under C:\Qt\Tools) -------------
CMAKE_BIN_UNIX="/c/Qt/Tools/CMake/bin"
NINJA_BIN_UNIX="/c/Qt/Tools/Ninja"

# --- MSVC (vcvars64) - import into THIS shell only ----------------------
VCVARSALL_WIN="C:\\Program Files\\Microsoft Visual Studio\\2022\\Community\\VC\\Auxiliary\\Build\\vcvars64.bat"
VSWHERE_DIR_WIN="C:\\Program Files (x86)\\Microsoft Visual Studio\\Installer"

# Run vcvars64.bat via a tiny generated .bat file (avoids fragile quoting of
# nested double-quotes / && when going through Git Bash -> cmd.exe), then
# capture and re-export every variable it sets.
_vcvars_capture_unix="$(mktemp --suffix=.bat)"
_vcvars_capture_win="$(cygpath -w "$_vcvars_capture_unix")"

cat > "$_vcvars_capture_unix" <<BATCH
@echo off
set "PATH=${VSWHERE_DIR_WIN};%PATH%"
call "${VCVARSALL_WIN}"
set
BATCH

_vcvars_env="$(cmd //c "$_vcvars_capture_win" 2>/dev/null)"
rm -f "$_vcvars_capture_unix"

# A valid POSIX/bash variable name: letters, digits, underscore, not
# starting with a digit. Windows exposes some names cmd.exe accepts but
# bash cannot (e.g. "ProgramFiles(x86)") -- silently skip those; they are
# not needed by cl.exe/link.exe, which read the Windows environment block
# directly rather than through bash.
_is_valid_name() { case "$1" in [A-Za-z_]*) [ -z "${1//[A-Za-z0-9_]/}" ] ;; *) false ;; esac; }

while IFS='=' read -r _key _value; do
    # cmd.exe's "set" output is CRLF-terminated; bash's `read` only splits
    # on \n, so a trailing \r would otherwise stick to every value (and
    # corrupt things like COMSPEC, which CMake/Ninja embed verbatim into
    # generated build files -- a stray \r there breaks Ninja's lexer).
    _key="${_key%$'\r'}"
    _value="${_value%$'\r'}"
    [ -z "$_key" ] && continue
    _is_valid_name "$_key" || continue

    if [ "$_key" = "PATH" ]; then
        # PATH comes back Windows-style (C:\...;C:\...). Convert each
        # entry to a Unix path and PREPEND to bash's existing PATH instead
        # of overwriting it -- overwriting would break bash's own PATH
        # (ls, cat, cygpath, etc. would stop resolving).
        _new_path_unix=""
        _old_ifs="$IFS"
        IFS=';'
        for _entry in $_value; do
            [ -z "$_entry" ] && continue
            _conv="$(cygpath -u "$_entry" 2>/dev/null)"
            [ -n "$_conv" ] && _new_path_unix="${_new_path_unix:+$_new_path_unix:}$_conv"
        done
        IFS="$_old_ifs"
        export PATH="${_new_path_unix}:${PATH}"
    elif [ -n "$_value" ]; then
        export "$_key=$_value"
    fi
done <<EOF
$_vcvars_env
EOF
unset _vcvars_env _vcvars_capture_unix _vcvars_capture_win _key _value _new_path_unix _entry _conv _old_ifs

# --- Qt (exported AFTER the vcvars import -- see note above) ------------
export QT_DIR="C:\\Qt\\6.8.3\\msvc2022_64"
QT_DIR_UNIX="/c/Qt/6.8.3/msvc2022_64"
export CMAKE_PREFIX_PATH="$QT_DIR"

# --- Prepend Qt/CMake/Ninja to PATH (process-local only) ---------------
export PATH="$QT_DIR_UNIX/bin:$CMAKE_BIN_UNIX:$NINJA_BIN_UNIX:$PATH"

echo "Reldex toolchain environment ready (this shell only):"
echo "  QT_DIR             = $QT_DIR"
echo "  CMAKE_PREFIX_PATH  = $CMAKE_PREFIX_PATH"
echo "  cmake              = $(command -v cmake || true)"
echo "  ninja              = $(command -v ninja || true)"
echo "  cl (MSVC)          = $(command -v cl || true)"
echo "  VCToolsInstallDir  = ${VCToolsInstallDir:-<not set>}"
