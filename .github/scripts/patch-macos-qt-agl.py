#!/usr/bin/env python3
"""Work around QTBUG-137687 in an installed Qt 6.8.3 (macOS only).

Apple removed the AGL framework from the macOS 26 (Tahoe) / Xcode 26 SDK
that GitHub's macos-latest runners now ship. Qt 6.8.3's own
FindWrapOpenGL.cmake still falls back to an unconditional `-framework AGL`
link flag when AGL can't be found via find_library(), which makes every Qt
Quick target fail to link:

    ld: framework 'AGL' not found

Tracked upstream as https://bugreports.qt.io/browse/QTBUG-137687 and fixed
in Qt 6.8.4 / 6.9.2. Neither is usable here: aqtinstall's open-source macOS
channel only listed Qt up to 6.8.3 / 6.9.3 as of 2026-09-20 (checked with
`aqt list-qt mac desktop --spec 6.8` / `--spec 6.9` -- 6.8.4 is simply not
published there), and moving to the 6.9 minor line is a version-policy
decision beyond M1.4's scope (reported, not decided, in that task's report).

This script reproduces the exact upstream fix against the installed copy.
Verified by diffing qt/qtbase's FindWrapOpenGL.cmake between v6.8.3 and
v6.9.2: the AGL block below is the only difference between the two files.
It fails loudly (non-zero exit) if the installed file's text does not match
byte-for-byte, rather than silently doing nothing -- if a future Qt 6.8.x
patch release changes this file, this script must be re-checked, not
trusted blindly.

Usage: python3 patch-macos-qt-agl.py <path-to-FindWrapOpenGL.cmake>
"""

import sys

OLD_BLOCK = (
    '        find_library(WrapOpenGL_AGL NAMES AGL)\n'
    '        if(WrapOpenGL_AGL)\n'
    '            set(__opengl_agl_fw_path "${WrapOpenGL_AGL}")\n'
    '        endif()\n'
    '        if(NOT __opengl_agl_fw_path)\n'
    '            set(__opengl_agl_fw_path "-framework AGL")\n'
    '        endif()\n'
    '\n'
    '        target_link_libraries(WrapOpenGL::WrapOpenGL INTERFACE ${__opengl_fw_path})\n'
    '        target_link_libraries(WrapOpenGL::WrapOpenGL INTERFACE ${__opengl_agl_fw_path})\n'
)

NEW_BLOCK = (
    '        target_link_libraries(WrapOpenGL::WrapOpenGL INTERFACE ${__opengl_fw_path})\n'
)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: patch-macos-qt-agl.py <path-to-FindWrapOpenGL.cmake>", file=sys.stderr)
        return 2

    path = sys.argv[1]
    # newline="" on both ends: read and write the file's bytes as-is (no
    # universal-newline translation), so this is exact regardless of host
    # platform, and never silently rewrites the file's line endings.
    with open(path, "r", encoding="utf-8", newline="") as f:
        text = f.read()

    count = text.count(OLD_BLOCK)
    if count != 1:
        print(
            f"patch-macos-qt-agl.py: expected exactly 1 occurrence of the known "
            f"QTBUG-137687 AGL block in {path}, found {count}. Qt's file layout "
            f"has apparently changed -- update this script (or delete it and the "
            f"workflow step that calls it, if the pinned Qt version already "
            f"fixes QTBUG-137687).",
            file=sys.stderr,
        )
        return 1

    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(text.replace(OLD_BLOCK, NEW_BLOCK))

    print(f"patch-macos-qt-agl.py: removed the AGL framework fallback from {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
