# The thin C++/Qt adapter, built as a QML module. AGENTS.md: "Keep the
# C++/Qt adapter thin" -- this library's only job is to call into reldex-ffi
# and expose the result as QObject properties and item models; no business
# logic.
qt_add_library(reldex_adapter STATIC)

# OUTPUT_DIRECTORY comes from the top-level QT_QML_OUTPUT_DIRECTORY (see
# ui/CMakeLists.txt for why that variable exists) -- it resolves to
# ${CMAKE_BINARY_DIR}/qml/Reldex/Adapter, which is what lets any consumer's
# `import Reldex.Adapter` find and statically link this module's plugin.
qt_add_qml_module(reldex_adapter
    URI Reldex.Adapter
    VERSION 1.0
    SOURCES
        CoreInfo.h
        CoreInfo.cpp
        ReldexHandles.h
        Metrics.h
        Metrics.cpp
        Bridge.h
        Bridge.cpp
        SessionController.h
        SessionController.cpp
        ResultTableModel.h
        ResultTableModel.cpp
)

target_include_directories(reldex_adapter PUBLIC
    "${RELDEX_FFI_INCLUDE_DIR}"
    "${CMAKE_CURRENT_SOURCE_DIR}"
)

target_link_libraries(reldex_adapter
    PUBLIC
        Qt6::Core
        Qt6::Gui
        Qt6::Qml
        # Quick, for QQuickWindow::frameSwapped -- the frame-interval recorder
        # spike S15's K1 needs (ui/adapter/Metrics.h). LGPL module, like the
        # rest (ui/README.md "Licence note").
        Qt6::Quick
        reldex_ffi-shared
)

reldex_enable_warnings(reldex_adapter)
