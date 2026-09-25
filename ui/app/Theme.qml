pragma Singleton
import QtQuick

import Reldex.Adapter

// `Theme` -- M3.1: the single source of semantic colour tokens for the app
// shell. `AGENTS.md` keeps business rules out of QML, but colour tokens are
// presentation, not business logic, so a QML singleton (rather than a C++
// class) is the right home -- it is the one place "no restart" (phase-1.md
// row M3.1's acceptance criterion) has to be proven: every consumer binds to
// `Theme.tokens.*`, so a single property change here re-evaluates every
// binding in the tree with no window or QQmlEngine recreation.
//
// Two independent inputs decide `isDark`, both live/reactive:
//  * `Application.styleHints.colorScheme` -- the OS-level light/dark
//    preference (Qt 6.5+, `QStyleHints::colorSchemeChanged`); followed by
//    default (SPEC.md §14 "light/dark themes").
//  * `AppSettings.themeOverride` -- System (default) / Light / Dark, the
//    user override. It is a plain in-memory C++ property today (see
//    `ui/adapter/AppSettings.h` for the TODO binding it to the real M2.9
//    settings model / M3.6 UI); this file does not care where the property
//    comes from, only that changing it re-evaluates `isDark` immediately.
QtObject {
    id: theme

    readonly property int systemScheme: Application.styleHints.colorScheme
    readonly property bool isDark: AppSettings.themeOverride === AppSettings.Dark
        || (AppSettings.themeOverride === AppSettings.System && systemScheme === Qt.Dark)

    // Semantic tokens, not raw colour names, so a consumer says what a
    // colour is *for* (SPEC.md §14, ARCHITECTURE.md invariant 4 -- QML stays
    // presentation, and a semantic token is what keeps a future design pass
    // from becoming a grep-and-replace across every .qml file).
    readonly property QtObject light: QtObject {
        readonly property color background: "#f4f5f7"
        readonly property color surface: "#ffffff"
        readonly property color surfaceAlt: "#eaecf1"
        readonly property color text: "#10131a"
        readonly property color textMuted: "#5b6472"
        readonly property color accent: "#1d4ed8"
        readonly property color accentText: "#ffffff"
        readonly property color border: "#d6dae1"
        readonly property color selection: "#c7d7fe"
        readonly property color error: "#b91c1c"
        readonly property color warning: "#b45309"
        readonly property color info: "#0369a1"
        readonly property color editorBackground: "#ffffff"
        readonly property color editorText: "#10131a"
        readonly property color editorLineNumber: "#94a3b8"
        readonly property color editorCurrentLine: "#eef2ff"
        readonly property color editorSelection: "#c7d7fe"
    }

    readonly property QtObject dark: QtObject {
        readonly property color background: "#14161c"
        readonly property color surface: "#1c1f27"
        readonly property color surfaceAlt: "#262a34"
        readonly property color text: "#e7eaf0"
        readonly property color textMuted: "#9aa4b2"
        readonly property color accent: "#6d93ff"
        readonly property color accentText: "#0b1021"
        readonly property color border: "#343a47"
        readonly property color selection: "#2c3a63"
        readonly property color error: "#f87171"
        readonly property color warning: "#fbbf24"
        readonly property color info: "#38bdf8"
        readonly property color editorBackground: "#101319"
        readonly property color editorText: "#e7eaf0"
        readonly property color editorLineNumber: "#5b6472"
        readonly property color editorCurrentLine: "#1e2430"
        readonly property color editorSelection: "#2c3a63"
    }

    readonly property QtObject tokens: isDark ? dark : light
}
