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

qt_add_qml_module(Reldex
    URI Reldex.App
    VERSION 1.0
    QML_FILES
        Main.qml
)

target_link_libraries(Reldex
    PRIVATE
        Qt6::Core
        Qt6::Gui
        Qt6::Qml
        Qt6::Quick
        reldex_adapter
)

reldex_enable_warnings(Reldex)
reldex_deploy_ffi_dll(Reldex)
