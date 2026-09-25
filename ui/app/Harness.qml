import QtQuick
import QtQuick.Window

import Reldex.Adapter

// The M1.6/M1.8 spike-S15 measurement harness. Through M3.1 this file *was*
// Main.qml -- the app's only window. M3.1 replaced Main.qml with the real
// app shell (Sidebar/WorksheetArea/OutputPanes/StatusBar) and moved this
// content here unchanged, per the task brief: "keep the S15 harness
// reachable ... so the perf scroll driver still works -- do not delete
// measurement code." `ui/app/main.cpp` loads this component instead of
// `Main` whenever the *existing* S15 env gate is set (`RELDEX_S15_AUTORUN`,
// `RELDEX_S15_SCROLL` or `RELDEX_S15_RUNS` -- every documented
// `ui/README.md` repro command already sets one of these) or `--harness` is
// passed on the command line. See `ui/README.md` "Running the shell vs the
// harness".
//
// M1.6 scope note (unchanged): the smallest QML surface that lets M1.8
// measure spike S15 -- a TableView over the real ResultTableModel with a
// plain Text delegate, and one way to run the mock generated query. No
// editor, no toolbar, no theming (it does not use `Theme` -- a measurement
// harness should not vary with the app shell's presentation work), and no
// settings: `run()` sequences open-then-execute in C++.
Window {
    id: root

    width: 960
    height: 640
    visible: true
    title: "Reldex (S15 harness)"

    readonly property var session: bridge.session

    Bridge {
        id: bridge

        // Named so a test can reach it from the loaded tree (ui/tests).
        objectName: "bridge"
    }

    Component.onCompleted: {
        bridge.metrics.attachWindow(root);
        // Spike S15's measurement driver (M1.8). Inert unless the environment
        // asks for a run; the sequencing lives in C++ so QML has none of it.
        bridge.scrollDriver.attach(root, table);
        bridge.autoStart();
    }

    Text {
        id: status

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: 8
        elide: Text.ElideRight
        font.pixelSize: 13
        text: bridge.valid
            ? "reldex-ffi ABI " + CoreInfo.abiVersion
              + "   state " + root.session.state
              + "   rows " + root.session.rowsFetched
              + (root.session.hasError ? "   error: " + root.session.errorMessage : "")
            : "reldex-ffi is unusable: ABI major mismatch"
    }

    Text {
        id: runButton

        anchors.left: parent.left
        anchors.top: status.bottom
        anchors.margins: 8
        font.pixelSize: 13
        font.underline: true
        color: "#1d4ed8"
        text: "Run the mock generated query"

        MouseArea {
            anchors.fill: parent
            cursorShape: Qt.PointingHandCursor
            onClicked: bridge.run()
        }
    }

    Row {
        id: header

        objectName: "header"
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: runButton.bottom
        anchors.margins: 8
        height: 24
        spacing: 0

        Repeater {
            // The model's notifying `columnNames`, not a `headerData()` call:
            // a binding on headerData() evaluates once, before the first batch
            // has brought the names, and never re-evaluates, because
            // headerDataChanged is not a property-change signal.
            model: bridge.valid ? root.session.model.columnNames : []

            delegate: Text {
                required property string modelData

                width: 220
                height: parent.height
                elide: Text.ElideRight
                font.bold: true
                font.pixelSize: 12
                leftPadding: 4
                verticalAlignment: Text.AlignVCenter
                text: modelData
            }
        }
    }

    TableView {
        id: table

        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: header.bottom
        anchors.bottom: parent.bottom
        anchors.margins: 8
        clip: true
        model: bridge.valid ? root.session.model : null

        columnWidthProvider: function (column) { return 220; }
        rowHeightProvider: function (row) { return 22; }

        delegate: Text {
            required property string display
            required property bool isNull

            elide: Text.ElideRight
            font.pixelSize: 12
            font.italic: isNull
            color: isNull ? "#94a3b8" : "#0f172a"
            leftPadding: 4
            verticalAlignment: Text.AlignVCenter
            text: display
        }
    }
}
