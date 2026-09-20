qt_add_executable(tst_coreinfo
    tst_coreinfo.cpp
)

# Re-attaches the *same* ../app/Main.qml file (not a copy) under the same
# "Reldex.App" URI, for the same reason ui/app/CMakeLists.txt attaches it
# directly to the `Reldex` executable rather than to a shared library: a
# statically linked QML entry-point module only self-registers reliably when
# compiled straight into the binary that loads it. Each executable's default
# qt_add_qml_module() OUTPUT_DIRECTORY is <its own CMAKE_CURRENT_BINARY_DIR>
# plus the target path, so Reldex and tst_coreinfo do not collide even
# though they share both the URI and the source file.
set(RELDEX_APP_MAIN_QML "${CMAKE_SOURCE_DIR}/app/Main.qml")
# QT_RESOURCE_ALIAS is required because the file is referenced by an
# absolute path from outside this directory; without it, qt_add_qml_module()
# refuses ("used in a Qt resource... set QT_RESOURCE_ALIAS").
set_source_files_properties(${RELDEX_APP_MAIN_QML} PROPERTIES
    QT_RESOURCE_ALIAS "Main.qml"
)

# NO_CACHEGEN: the AOT qmlcache compiler derives its intermediate file names
# from the QML_FILES path relative to this directory (`../app/Main.qml`),
# which it cannot turn into a valid output path when the source lives
# outside this target's own source tree. Not needed for a test binary --
# it only affects load-time compilation, not what is being tested here.
#
# OUTPUT_DIRECTORY is overridden (instead of taking the ui/CMakeLists.txt
# default of ${QT_QML_OUTPUT_DIRECTORY}/Reldex/App) because ui/app's own
# "Reldex.App" module already claims that exact directory; two different
# targets are not allowed to share one module output directory.
qt_add_qml_module(tst_coreinfo
    URI Reldex.App
    VERSION 1.0
    NO_CACHEGEN
    OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/qml-tests/Reldex/App"
    QML_FILES
        ${RELDEX_APP_MAIN_QML}
)

target_link_libraries(tst_coreinfo
    PRIVATE
        Qt6::Core
        Qt6::Gui
        Qt6::Qml
        Qt6::Quick
        Qt6::Test
        reldex_adapter
)

reldex_enable_warnings(tst_coreinfo)
reldex_deploy_ffi_dll(tst_coreinfo)

add_test(NAME tst_coreinfo COMMAND tst_coreinfo)
set_tests_properties(tst_coreinfo PROPERTIES
    ENVIRONMENT "QT_QPA_PLATFORM=offscreen"
)

# M1.4: the Qt-free C/C++ smoke harness (ADR-0003 D10 item 2). Reuses the
# reldex_ffi-shared target and RELDEX_FFI_INCLUDE_DIR that ui/CMakeLists.txt
# already imported/set; see ui/tests/ffi_smoke/CMakeLists.txt for the guard
# that keeps this from double-importing Corrosion when built standalone.
add_subdirectory(ffi_smoke)
