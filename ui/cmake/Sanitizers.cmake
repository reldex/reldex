# RELDEX_SANITIZE (declared in ui/CMakeLists.txt, immediately before this
# file is include()'d there) instruments every target this project's
# CMakeLists.txt subtree builds -- reldex_adapter, the Reldex app, and every
# QTest binary -- with ASan+UBSan on GCC/Clang. It does this with plain
# directory-scoped add_compile_options()/add_link_options() in the top-level
# ui/CMakeLists.txt: CMake directory properties are inherited by every
# add_subdirectory()'d child added afterwards, so nothing in ui/adapter,
# ui/app or ui/tests has to opt in per target.
#
# ui/tests/ffi_smoke/CMakeLists.txt declares its own copy of this same
# RELDEX_SANITIZE option name and applies it per target explicitly -- kept
# separate because that directory also builds fully standalone, with no
# ui/CMakeLists.txt at all. When it is instead add_subdirectory()'d in from
# here, its two targets end up instrumented twice over (this directory-scoped
# application, inherited, plus its own explicit one); GCC/Clang both tolerate
# the resulting duplicate -fsanitize flags, so this is left alone rather than
# special-cased.
#
# This never touches Qt itself (found prebuilt via find_package(), never
# recompiled here) or reldex-ffi's Rust cdylib (an IMPORTED target that
# Corrosion builds by invoking cargo directly, not one CMake compiles itself,
# so a directory-scoped compile option never reaches it) -- but ASan's
# runtime, once linked into the final executable, still intercepts
# malloc/free calls made by uninstrumented code running in the same process.
#
# ADR-0003 kill criterion K5: "Waker teardown: destroy the C++ bridge under a
# flood of completions, 10,000 iterations, ASan -- any use-after-free, race,
# or hang." The test (ui/tests/tst_teardown.cpp) and its live-count
# assertions already exist; this is what makes running it under ASan
# possible at all (see ui/README.md "AddressSanitizer: not available on this
# machine" for why it could not be proven on the Windows dev machine).
if(RELDEX_SANITIZE)
    if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang" OR CMAKE_C_COMPILER_ID MATCHES "GNU|Clang")
        add_compile_options(-fsanitize=address,undefined -fno-omit-frame-pointer -g)
        add_link_options(-fsanitize=address,undefined)
    else()
        message(WARNING
            "RELDEX_SANITIZE=ON but the compiler is neither GCC nor Clang; "
            "ignoring (MSVC's ASan does not cover UBSan and is not wired up "
            "here -- same rule ui/tests/ffi_smoke/CMakeLists.txt applies to "
            "its own targets).")
    endif()
endif()
