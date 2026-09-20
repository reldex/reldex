# RELDEX_SANITIZE (declared in ui/CMakeLists.txt, project-wide) instruments
# every target THIS project builds -- reldex_adapter, the Reldex app, and
# every QTest binary -- with ASan+UBSan on GCC/Clang. It reuses the same
# option name ui/tests/ffi_smoke/CMakeLists.txt already declares for its own
# (Qt-free) targets: that directory keeps its own copy because it also builds
# standalone (its own project(), no parent ui/CMakeLists.txt at all), but
# option() never overrides an already-cached value, so configuring the
# top-level project with -DRELDEX_SANITIZE=ON makes ffi_smoke's targets pick
# up the very same setting when it is add_subdirectory()'d in.
#
# This never touches Qt itself (found prebuilt via find_package(), never
# recompiled here) or reldex-ffi's Rust cdylib (built by cargo via Corrosion,
# a completely separate build step) -- but ASan's runtime, once linked into
# the final executable, still intercepts malloc/free calls made by
# uninstrumented code running in the same process, exactly as
# ui/tests/ffi_smoke/CMakeLists.txt's own comment already documents for its
# harness.
#
# ADR-0003 kill criterion K5: "Waker teardown: destroy the C++ bridge under a
# flood of completions, 10,000 iterations, ASan -- any use-after-free, race,
# or hang." The test (ui/tests/tst_teardown.cpp) and its live-count assertions
# already exist; this is what makes running it under ASan possible at all
# (see ui/README.md "AddressSanitizer: not available on this machine" for why
# it could not be proven on the Windows dev machine, and TASKS.md/the active
# plan for where this CI leg is tracked).
function(reldex_enable_sanitizers target)
    if(NOT RELDEX_SANITIZE)
        return()
    endif()

    if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang" OR CMAKE_C_COMPILER_ID MATCHES "GNU|Clang")
        target_compile_options(${target} PRIVATE -fsanitize=address,undefined -fno-omit-frame-pointer -g)
        target_link_options(${target} PRIVATE -fsanitize=address,undefined)
    else()
        message(WARNING
            "RELDEX_SANITIZE=ON but ${target}'s compiler is neither GCC nor "
            "Clang; ignoring (MSVC's ASan does not cover UBSan and is not "
            "wired up here -- same rule ui/tests/ffi_smoke/CMakeLists.txt "
            "applies to its own targets).")
    endif()
endfunction()
