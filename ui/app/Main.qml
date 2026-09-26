import QtQuick
import QtQuick.Window
import QtQuick.Controls

import Reldex.Adapter

// The Reldex app shell (M3.1): a fixed, docking-free layout -- left sidebar,
// centre worksheet tab bar + content, bottom output panes, status bar --
// wired to `Theme` (light/dark tokens, live system-scheme follow + user
// override, no restart) and sized entirely in device-independent pixels
// (SPEC.md §14/§19; every dimension here is a plain number, which Qt Quick
// always treats as DIPs -- high-DPI scaling is the platform's job, not
// this file's).
//
// Presentation only, per ARCHITECTURE.md invariant 4: no business rules
// here. Every backend-shaped placeholder (session state, connections,
// results, DBMS_OUTPUT, production indicator) names the milestone that
// replaces it. See `ui/README.md` "App shell" for the layout diagram and
// how this differs from `Harness.qml`, the still-reachable S15 measurement
// window (`ui/app/main.cpp` picks between the two).
ApplicationWindow {
    id: root

    width: 1280
    height: 800
    minimumWidth: 900
    minimumHeight: 560
    visible: true

    // The brand name (AGENTS.md "Naming and branding") -- deliberately not
    // wrapped in qsTr(): a product name is never translated, unlike every
    // other user-visible string in this file. Named explicitly, rather than
    // inlined into `title` below, so that not-translating it reads as a
    // decision here rather than an oversight next to all the qsTr() calls
    // around it.
    readonly property string productName: "Reldex"

    title: root.productName
    color: Theme.tokens.background

    // Session-only, per the task brief: resets to expanded on every launch.
    // Persisting this is M6.2 ("UI layout" in SPEC.md §20's local-persistence
    // list), not M3.1.
    property bool sidebarVisible: true
    property bool outputPaneVisible: true

    function toggleSidebar() { root.sidebarVisible = !root.sidebarVisible }
    function toggleOutputPane() { root.outputPaneVisible = !root.outputPaneVisible }

    // M3.2: the app's one Bridge (one hub, one workspace service thread --
    // ADR-0006 P6). Every other milestone's placeholder in this shell
    // (session state, results, DBMS_OUTPUT, the production indicator) names
    // itself as the reason this was not created in M3.1; the connection
    // manager is the first of those to actually need it. Named the same way
    // Harness.qml already does ("bridge"), so a test can reach it the same
    // way there.
    Bridge {
        id: bridge
        objectName: "bridge"
    }

    // VSCode's own bindings for the same two affordances (sidebar / bottom
    // panel), chosen because they are already muscle memory for a large
    // share of this product's target users.
    Shortcut {
        sequence: "Ctrl+B"
        onActivated: root.toggleSidebar()
    }

    Shortcut {
        sequence: "Ctrl+J"
        onActivated: root.toggleOutputPane()
    }

    SplitView {
        id: outerSplit
        objectName: "outerSplit"
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: statusBar.top
        orientation: Qt.Horizontal

        handle: Rectangle {
            implicitWidth: 4
            implicitHeight: 4
            color: SplitHandle.pressed ? Theme.tokens.accent : Theme.tokens.border
        }

        Sidebar {
            id: sidebarPane
            SplitView.preferredWidth: 240
            SplitView.minimumWidth: 160
            SplitView.maximumWidth: 480
            visible: root.sidebarVisible
            connectionManager: bridge.connections
        }

        SplitView {
            id: innerSplit
            objectName: "innerSplit"
            orientation: Qt.Vertical
            SplitView.fillWidth: true

            handle: Rectangle {
                implicitWidth: 4
                implicitHeight: 4
                color: SplitHandle.pressed ? Theme.tokens.accent : Theme.tokens.border
            }

            WorksheetArea {
                id: worksheetAreaPane
                SplitView.fillHeight: true
                SplitView.minimumHeight: 120
            }

            OutputPanes {
                id: outputPanesPane
                SplitView.preferredHeight: 170
                SplitView.minimumHeight: 60
                visible: root.outputPaneVisible
            }
        }
    }

    StatusBar {
        id: statusBar
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        height: 28
    }
}
