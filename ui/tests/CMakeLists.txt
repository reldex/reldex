qt_add_executable(tst_coreinfo
    tst_coreinfo.cpp
    AdapterTestSupport.h
)
# See reldex_add_adapter_test() below for why the console subsystem is forced.
set_target_properties(tst_coreinfo PROPERTIES
    WIN32_EXECUTABLE FALSE
    MACOSX_BUNDLE FALSE
)

# Re-attaches the *same* ../app/*.qml files (not copies) under the same
# "Reldex.App" URI, for the same reason ui/app/CMakeLists.txt attaches them
# directly to the `Reldex` executable rather than to a shared library: a
# statically linked QML entry-point module only self-registers reliably when
# compiled straight into the binary that loads it. Each executable's default
# qt_add_qml_module() OUTPUT_DIRECTORY is <its own CMAKE_CURRENT_BINARY_DIR>
# plus the target path, so Reldex and tst_coreinfo do not collide even
# though they share both the URI and every source file. RELDEX_APP_QML_FILES
# is the single list ui/CMakeLists.txt defines for exactly this reuse (M3.1).
#
# RELDEX_APP_QML_FILES (ui/CMakeLists.txt) holds bare filenames, relative to
# ui/app/ -- resolved to absolute paths here since this directory is not
# ui/app. QT_RESOURCE_ALIAS is required per file because each is then
# referenced by an absolute path from outside this directory; without it,
# qt_add_qml_module() refuses ("used in a Qt resource... set
# QT_RESOURCE_ALIAS").
set(RELDEX_APP_QML_ABSOLUTE_FILES "")
foreach(reldexAppQmlFileName IN LISTS RELDEX_APP_QML_FILES)
    set(reldexAppQmlFile "${CMAKE_SOURCE_DIR}/app/${reldexAppQmlFileName}")
    set_source_files_properties(${reldexAppQmlFile} PROPERTIES
        QT_RESOURCE_ALIAS "${reldexAppQmlFileName}"
    )
    list(APPEND RELDEX_APP_QML_ABSOLUTE_FILES "${reldexAppQmlFile}")
endforeach()

# See ui/app/CMakeLists.txt for why this is required (not auto-detected from
# Theme.qml's own `pragma Singleton`): this module is compiled separately
# into tst_coreinfo, so it needs the same source property set again here.
set_source_files_properties("${CMAKE_SOURCE_DIR}/app/Theme.qml" PROPERTIES
    QT_QML_SINGLETON_TYPE TRUE
)

# NO_CACHEGEN: the AOT qmlcache compiler derives its intermediate file names
# from each QML_FILES path relative to this directory (`../app/Main.qml`),
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
        ${RELDEX_APP_QML_ABSOLUTE_FILES}
)

target_link_libraries(tst_coreinfo
    PRIVATE
        Qt6::Core
        Qt6::Gui
        Qt6::Qml
        Qt6::Quick
        # QQuickStyle (M3.1): tst_coreinfo.cpp sets the Basic style itself in
        # initTestCase(), for the same reason and with the same "before the
        # first QML load" ordering as ui/app/main.cpp.
        Qt6::QuickControls2
        Qt6::Test
        reldex_adapter
)

reldex_enable_warnings(tst_coreinfo)
reldex_deploy_ffi_dll(tst_coreinfo)

add_test(NAME tst_coreinfo COMMAND tst_coreinfo)
set_tests_properties(tst_coreinfo PROPERTIES
    ENVIRONMENT "QT_QPA_PLATFORM=offscreen"
)

# M3.1 acceptance (docs/exec-plans/active/phase-1.md row M3.1): "Renders at
# 100/150/200% DPI." QT_SCALE_FACTOR is read once at QPA platform start-up
# (see appShellRendersAtCurrentScaleFactor() in tst_coreinfo.cpp), so it
# cannot be varied within one already-running process -- these are three
# separate ctest processes, each running only that one test function, rather
# than three assertions inside a single run.
foreach(reldexDpiPercent IN ITEMS 100 150 200)
    if(reldexDpiPercent STREQUAL "100")
        set(reldexScaleFactor "1")
    elseif(reldexDpiPercent STREQUAL "150")
        set(reldexScaleFactor "1.5")
    else()
        set(reldexScaleFactor "2")
    endif()
    add_test(NAME tst_appshell_dpi${reldexDpiPercent}
             COMMAND tst_coreinfo appShellRendersAtCurrentScaleFactor)
    set_tests_properties(tst_appshell_dpi${reldexDpiPercent} PROPERTIES
        ENVIRONMENT "QT_QPA_PLATFORM=offscreen;QT_SCALE_FACTOR=${reldexScaleFactor}"
    )
endforeach()

# M3.2 fix round should-fix (2026-09-26): DPI coverage for the connection-
# manager dialog, at 1280x800 (asserted inside the function itself, which
# also runs once, unfiltered, as part of the plain "tst_coreinfo" entry above
# at the default 1x) and 2x, mirroring the app-shell DPI registrations above
# without needing a third/fourth process for 1x again.
add_test(NAME tst_connectionmanagerdialog_dpi200
         COMMAND tst_coreinfo connectionManagerDialogRendersAtCurrentScaleFactor)
set_tests_properties(tst_connectionmanagerdialog_dpi200 PROPERTIES
    ENVIRONMENT "QT_QPA_PLATFORM=offscreen;QT_SCALE_FACTOR=2"
)

# M1.4: the Qt-free C/C++ smoke harness (ADR-0003 D10 item 2). Reuses the
# reldex_ffi-shared target and RELDEX_FFI_INCLUDE_DIR that ui/CMakeLists.txt
# already imported/set; see ui/tests/ffi_smoke/CMakeLists.txt for the guard
# that keeps this from double-importing Corrosion when built standalone.
add_subdirectory(ffi_smoke)

# ---------------------------------------------------------------------------
# M1.6 adapter tests (Bridge / SessionController / ResultTableModel).
#
# These are console-subsystem QTest binaries with no QML and no window
# (QTEST_GUILESS_MAIN), so they need no platform plugin and run identically
# on a CI runner and on a desktop. The QML half of M1.6 -- Main.qml's
# TableView over ResultTableModel -- is covered by tst_coreinfo above, which
# loads that exact file offscreen and fails on any QML warning.
# ---------------------------------------------------------------------------
function(reldex_add_adapter_test name)
    qt_add_executable(${name} ${name}.cpp AdapterTestSupport.h)
    # qt_add_executable() sets WIN32_EXECUTABLE TRUE by default, which makes a
    # Windows binary GUI-subsystem -- and a GUI-subsystem process has no
    # console, so QTest's report and every qInfo() (including K5's iteration
    # count and timing) vanish instead of reaching CTest's log. These are test
    # binaries: force the console subsystem so their output is readable.
    #
    # MACOSX_BUNDLE FALSE is the same fix on the other platform: qt_add_executable()
    # also defaults it to TRUE, and a .app bundle puts the binary somewhere
    # `add_test(COMMAND ${name})` does not look.
    set_target_properties(${name} PROPERTIES
        WIN32_EXECUTABLE FALSE
        MACOSX_BUNDLE FALSE
    )
    target_link_libraries(${name}
        PRIVATE
            Qt6::Core
            Qt6::Test
            reldex_adapter
    )
    reldex_enable_warnings(${name})
    reldex_deploy_ffi_dll(${name})
    add_test(NAME ${name} COMMAND ${name})
endfunction()

reldex_add_adapter_test(tst_bridge)
reldex_add_adapter_test(tst_resultmodel)
reldex_add_adapter_test(tst_teardown)
# M3.2: ConnectionManager/ProfileModel -- profile list round-trip, the
# credential-pattern refusal, environment/production flag rules, ADR-0007's
# write-order/clearing choreography, and test-connect against the mock
# driver. One test opens a real on-disk store (a QTemporaryDir, never the
# developer's own); every other one is in-memory (RELDEX_WORKSPACE_IN_MEMORY).
reldex_add_adapter_test(tst_connectionmanager)

# K5 is 10,000 teardown iterations plus 2,000 connect-teardown iterations; the
# default CTest timeout of 1500 s is generous but this is the one test that
# could plausibly approach it on a slow machine, so it says what it needs.
set_tests_properties(tst_teardown PROPERTIES TIMEOUT 1800)
