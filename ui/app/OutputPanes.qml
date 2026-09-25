import QtQuick
import QtQuick.Controls

// Bottom output panes (M3.1): a tab bar over Messages and DBMS_OUTPUT
// (M4.7 builds the real DBMS_OUTPUT stream). Collapsible as a whole via
// Main.qml's Ctrl+J shortcut. Presentation only -- ARCHITECTURE.md
// invariant 4.
Rectangle {
    id: outputPanes
    objectName: "outputPanes"
    color: Theme.tokens.surface

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Output panes")

    Column {
        anchors.fill: parent

        TabBar {
            id: outputTabBar
            objectName: "outputTabBar"
            width: parent.width

            // See WorksheetArea.qml's tabBar for why these four roles
            // specifically (Basic's TabButton.qml reads them, not
            // button/buttonText/highlight) and the computed contrast ratios
            // (also in ui/README.md "App shell (M3.1)").
            palette.window: Theme.tokens.accent
            palette.windowText: Theme.tokens.accentText
            palette.dark: Theme.tokens.surfaceAlt
            palette.brightText: Theme.tokens.textMuted
            palette.mid: Theme.tokens.selection

            TabButton { text: qsTr("Messages") }
            TabButton { text: qsTr("DBMS_OUTPUT") }
        }

        Item {
            width: parent.width
            height: parent.height - outputTabBar.height

            Text {
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 8
                visible: outputTabBar.currentIndex === 0
                text: qsTr("No messages yet.")
                color: Theme.tokens.textMuted
                font.pixelSize: 12
            }

            Text {
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 8
                visible: outputTabBar.currentIndex === 1
                // M4.7 builds the real DBMS_OUTPUT stream.
                text: qsTr("DBMS_OUTPUT placeholder — M4.7")
                color: Theme.tokens.textMuted
                font.pixelSize: 12
            }
        }
    }
}
