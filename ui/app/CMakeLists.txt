# Main.qml is attached directly to the `Reldex` executable (rather than to a
# separate static library) because a *statically linked entry-point* QML
# module only self-registers reliably when it is compiled straight into the
# binary that loads it via loadFromModule(). A module living in its own
# static library is never referenced by symbol from main.cpp, so the linker
# is free to drop its whole translation unit -- confirmed by hand: splitting
# this into a `reldex_app_qml` library made `loadFromModule("Reldex.App",
# ...)` fail at runtime with "No module named Reldex.App found", even though
# the build succeeded warning-free. ui/tests/CMakeLists.txt re-attaches the
# same Main.qml file to the test executable for exactly this reason.
#
# `WIN32` is CMake's own add_executable() keyword for a GUI-subsystem binary
# on Windows (it sets the WIN32_EXECUTABLE target property) -- the analogue
# of MACOSX_BUNDLE on Apple, which qt_add_executable() also understands.
qt_add_executable(Reldex
    WIN32
    main.cpp
)

# `pragma Singleton` inside Theme.qml is documentation, not registration:
# qt_add_qml_module()'s qmldir generator only emits the `singleton` keyword
# when the QT_QML_SINGLETON_TYPE source property is set (checked directly in
# Qt6QmlMacros.cmake), so it has to be set explicitly here. Without it,
# `Theme` is registered as an ordinary (non-singleton) QML type, and every
# `Theme.tokens...` reference elsewhere silently resolves to `undefined`
# instead of failing to build (M3.1 build-time finding).
set_source_files_properties("${CMAKE_CURRENT_SOURCE_DIR}/Theme.qml" PROPERTIES
    QT_QML_SINGLETON_TYPE TRUE
)

qt_add_qml_module(Reldex
    URI Reldex.App
    VERSION 1.0
    QML_FILES
        ${RELDEX_APP_QML_FILES}
)

target_link_libraries(Reldex
    PRIVATE
        Qt6::Core
        Qt6::Gui
        Qt6::Qml
        Qt6::Quick
        # QQuickStyle (main.cpp, M3.1): selects the Basic style before the
        # first QML load. The style's own QML plugin (Basic) is resolved at
        # runtime like any other Qt Quick import, from the Qt install's own
        # qml/ directory -- see ui/README.md "App shell" for why no further
        # CMake component is needed for that half.
        Qt6::QuickControls2
        reldex_adapter
)

reldex_enable_warnings(Reldex)
reldex_deploy_ffi_dll(Reldex)
