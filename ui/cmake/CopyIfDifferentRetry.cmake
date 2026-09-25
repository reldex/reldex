# ui/cmake/CopyIfDifferentRetry.cmake
#
# `cmake -E copy_if_different <SRC> <DST>` wrapped with a retry, to absorb a
# transient "source not found" race observed in M1.4's CI (ui.yml, qt-build,
# macos-latest): under enough ninja parallelism (the full ui/ build links
# several targets against reldex_ffi-shared, each with its own POST_BUILD
# copy of the same source file -- Reldex, tst_coreinfo, reldex_ffi_smoke_c,
# reldex_ffi_smoke_cpp), the copy occasionally reported the source dylib as
# missing seconds after this same target had just linked successfully
# against that exact file. Not reproduced on windows-latest or ubuntu-latest,
# and not reproduced by ui/tests/ffi_smoke's own much smaller standalone
# build (too few targets to expose the race) -- only the full multi-target
# macOS build. The exact underlying cause (ninja/Corrosion/APFS interaction)
# was not root-caused; a `POST_BUILD` custom command has no formal ninja
# "OUTPUT", so nothing here enforces ordering between two different targets'
# POST_BUILD steps beyond "runs after its own target links", which is not
# enough to explain what was observed. Retrying is a pragmatic, contained
# fix: it does not touch reldex-ffi, Corrosion, or the build graph, and it
# still fails loudly (FATAL_ERROR) if the source is genuinely never produced.
#
# 2026-09-25 (PR #34, ui.yml qt-build (macos-latest), run 36083920385 attempt
# 1): the same race recurred, this time exposing a TOCTOU gap in the retry
# above -- tests/tst_resultmodel's POST_BUILD step reached the `while(NOT
# EXISTS ...)` loop below, saw the source dylib already present (it had been
# produced ~17s earlier, at ninja step 17/93, and nothing in this build ever
# deletes or rewrites it), yet the single `copy_if_different` call
# immediately after that check still failed with "No such file or
# directory". One EXISTS check followed by exactly one copy attempt cannot
# absorb a source that is momentarily inaccessible for the copy itself (as
# opposed to merely not-yet-created) -- so the whole check-then-copy
# operation now retries as a unit, on the same ~10s/40-attempt budget, and
# only gives up (FATAL_ERROR) once that budget is exhausted. This still does
# not root-cause the underlying ninja/Corrosion/APFS interaction; it only
# closes the gap in this script's own mitigation for it.
#
# Usage: cmake -DSRC=<file> -DDST=<dir-or-file> -P CopyIfDifferentRetry.cmake

if(NOT DEFINED SRC OR NOT DEFINED DST)
    message(FATAL_ERROR "CopyIfDifferentRetry.cmake requires -DSRC=<file> -DDST=<dir-or-file>")
endif()

set(_attempt 0)
set(_max_attempts 40) # 40 * 0.25s = up to 10s -- generous, but not a hang.
set(_copy_result 1)
set(_copy_error "")

while(NOT _copy_result EQUAL 0 AND _attempt LESS _max_attempts)
    if(EXISTS "${SRC}")
        execute_process(
            COMMAND "${CMAKE_COMMAND}" -E copy_if_different "${SRC}" "${DST}"
            RESULT_VARIABLE _copy_result
            ERROR_VARIABLE _copy_error
        )
        if(_copy_result EQUAL 0)
            break()
        endif()
    endif()
    execute_process(COMMAND "${CMAKE_COMMAND}" -E sleep 0.25)
    math(EXPR _attempt "${_attempt} + 1")
endwhile()

if(NOT _copy_result EQUAL 0)
    if(NOT EXISTS "${SRC}")
        message(FATAL_ERROR
            "CopyIfDifferentRetry.cmake: '${SRC}' still does not exist after "
            "${_max_attempts} retries (~10s). This is not the transient race "
            "this script exists to absorb -- something did not build ${SRC} at all.")
    endif()
    message(FATAL_ERROR
        "CopyIfDifferentRetry.cmake: copy_if_different failed (${_copy_result}) "
        "for '${SRC}' -> '${DST}' after ${_attempt} retries (~10s): ${_copy_error}")
endif()
