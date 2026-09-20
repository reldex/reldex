# ui/tests/ffi_smoke -- a plain C11 program (also built as C++17, from the
# same source, to prove the header is C++-clean) that links reldex-ffi's
# cdylib directly and drives the mock driver end to end. NO Qt anywhere in
# this directory (ADR-0003 D10 item 2; M1.4).
#
# Works two ways:
#   1. Standalone: `cmake -S ui/tests/ffi_smoke -B <build-dir>`. Has its own
#      project() and imports Corrosion itself, at the same pinned commit
#      ui/CMakeLists.txt uses.
#   2. As `add_subdirectory(ffi_smoke)` from ui/tests/CMakeLists.txt (which
#      is itself add_subdirectory'd from ui/CMakeLists.txt), reusing the
#      Corrosion import and RELDEX_FFI_INCLUDE_DIR the parent already set up.
# The two must never both import Corrosion in the same configure run --
# CMAKE_SOURCE_DIR == CMAKE_CURRENT_SOURCE_DIR is the standard way to tell
# "am I the top-level CMakeLists.txt this run" from "was I add_subdirectory'd
# into someone else's project".

cmake_minimum_required(VERSION 3.24)

set(RELDEX_FFI_SMOKE_STANDALONE FALSE)
if(CMAKE_SOURCE_DIR STREQUAL CMAKE_CURRENT_SOURCE_DIR)
    set(RELDEX_FFI_SMOKE_STANDALONE TRUE)
endif()

if(RELDEX_FFI_SMOKE_STANDALONE)
    project(ReldexFfiSmoke
        DESCRIPTION "Qt-free C/C++ smoke harness for reldex-ffi"
        LANGUAGES C CXX
    )
    enable_testing()

    # Same pin as ui/CMakeLists.txt -- see ui/README.md "Dependencies" for
    # why Corrosion is used and why this exact commit was chosen. Duplicated
    # here (rather than shared) because this branch only runs when this
    # directory is the top-level project and ui/CMakeLists.txt is not even
    # being read.
    include(FetchContent)
    set(CORROSION_GIT_TAG "v0.6.1")
    set(CORROSION_GIT_COMMIT "1499b14e4906a2890f5cee1547c8848db261753d")
    FetchContent_Declare(
        Corrosion
        GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
        GIT_TAG ${CORROSION_GIT_COMMIT}
    )
    FetchContent_MakeAvailable(Corrosion)

    set(RELDEX_WORKSPACE_MANIFEST "${CMAKE_CURRENT_SOURCE_DIR}/../../../Cargo.toml")
    set(RELDEX_FFI_INCLUDE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../../crates/ffi/include")

    corrosion_import_crate(
        MANIFEST_PATH ${RELDEX_WORKSPACE_MANIFEST}
        CRATES reldex-ffi
    )
endif()

if(NOT TARGET reldex_ffi-shared)
    message(FATAL_ERROR
        "reldex_ffi-shared is not available. Standalone, this file imports it "
        "itself via Corrosion; via add_subdirectory, the parent "
        "(ui/CMakeLists.txt) must import it first.")
endif()

if(NOT DEFINED RELDEX_FFI_INCLUDE_DIR)
    message(FATAL_ERROR "RELDEX_FFI_INCLUDE_DIR must be set by whoever included this file.")
endif()

option(RELDEX_SANITIZE
    "Build the C/C++ smoke harness with -fsanitize=address,undefined (GCC/Clang only; reldex-ffi itself is never instrumented, but ASan still intercepts its malloc/free from this process)."
    OFF
)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_C_EXTENSIONS OFF)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# One shared implementation, compiled twice: once as smoke.c (C11) and once
# as a byte-identical copy with a .cpp extension (C++17), generated at
# configure time. CMake picks the compiler by extension, so this avoids
# fighting over a single source file's LANGUAGE property between two targets
# and avoids maintaining the logic twice.
set(RELDEX_FFI_SMOKE_CPP_COPY "${CMAKE_CURRENT_BINARY_DIR}/generated/smoke.cpp")
configure_file(
    "${CMAKE_CURRENT_SOURCE_DIR}/smoke.c"
    "${RELDEX_FFI_SMOKE_CPP_COPY}"
    COPYONLY
)

function(reldex_ffi_smoke_add_target target_name)
    add_executable(${target_name} ${ARGN})

    # This directory is Qt-free by design (ADR-0003 D10 item 2), but when
    # added via add_subdirectory the parent (ui/CMakeLists.txt) has already
    # set CMAKE_AUTOMOC/AUTORCC ON as directory-scoped variables, which are
    # inherited here and would otherwise run moc over these plain C/C++
    # targets for no reason -- observed in practice to also flip the C
    # target's linker language from C to CXX, once moc's generated
    # mocs_compilation.cpp gave it a C++ source file. Explicit OFF makes
    # both build modes (standalone and add_subdirectory) behave identically.
    set_target_properties(${target_name} PROPERTIES
        AUTOMOC OFF
        AUTORCC OFF
        AUTOUIC OFF
    )

    target_include_directories(${target_name} PRIVATE "${RELDEX_FFI_INCLUDE_DIR}")
    target_link_libraries(${target_name} PRIVATE reldex_ffi-shared)

    if(MSVC)
        target_compile_options(${target_name} PRIVATE /W4)
    else()
        target_compile_options(${target_name} PRIVATE -Wall -Wextra)
    endif()

    if(RELDEX_SANITIZE)
        if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang" OR CMAKE_C_COMPILER_ID MATCHES "GNU|Clang")
            target_compile_options(${target_name} PRIVATE -fsanitize=address,undefined -fno-omit-frame-pointer -g)
            target_link_options(${target_name} PRIVATE -fsanitize=address,undefined)
        else()
            message(WARNING "RELDEX_SANITIZE=ON but ${target_name}'s compiler is neither GCC nor Clang; ignoring (MSVC's ASan does not cover UBSan and is not wired up here).")
        endif()
    endif()

    # Windows has no rpath: reldex_ffi.dll must sit next to the executable
    # (mirrors ui/CMakeLists.txt's reldex_deploy_ffi_dll, duplicated here so
    # this directory has no hard dependency on the parent existing).
    add_custom_command(TARGET ${target_name} POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
                "$<TARGET_FILE:reldex_ffi-shared>"
                "$<TARGET_FILE_DIR:${target_name}>"
        COMMENT "Deploying reldex_ffi shared library next to ${target_name}"
        VERBATIM
    )

    add_test(NAME ${target_name} COMMAND ${target_name})
endfunction()

reldex_ffi_smoke_add_target(reldex_ffi_smoke_c "${CMAKE_CURRENT_SOURCE_DIR}/smoke.c")
reldex_ffi_smoke_add_target(reldex_ffi_smoke_cpp "${RELDEX_FFI_SMOKE_CPP_COPY}")
