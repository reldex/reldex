import QtQuick
import QtQuick.Controls

// Centre worksheet area (M3.1): a tab bar over worksheet tabs, each tab's
// content a draggable vertical split between an editor placeholder (M4.1
// builds the real SQL/PL-SQL editor) and a result-grid placeholder (M4.x).
// Tab add/remove here is pure UI state -- no session is opened, no
// `SessionController` exists yet (that is M3.3/M4.9's "N sessions, per-tab
// state" work). Presentation only -- ARCHITECTURE.md invariant 4.
Rectangle {
    id: worksheetArea
    objectName: "worksheetArea"
    color: Theme.tokens.background

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Worksheet")

    ListModel {
        id: tabsModel
    }

    Component.onCompleted: tabsModel.append({ label: qsTr("Worksheet 1") })

    function addTab() {
        tabsModel.append({ label: qsTr("Worksheet %1").arg(tabsModel.count + 1) })
        tabBar.currentIndex = tabsModel.count - 1
    }

    Column {
        anchors.fill: parent

        Rectangle {
            id: tabBarRow
            width: parent.width
            height: 32
            color: Theme.tokens.surfaceAlt

            Row {
                anchors.fill: parent

                TabBar {
                    id: tabBar
                    objectName: "worksheetTabBar"
                    width: parent.width - addButton.width
                    height: parent.height
                    background: Rectangle { color: "transparent" }

                    // Basic's own TabButton.qml (QtQuick/Controls/Basic/TabButton.qml)
                    // reads exactly these four palette roles -- `button`/`buttonText`/
                    // `highlight` (set here in an earlier revision) are not read by it
                    // at all, which is what left the *unselected* tab on Basic's
                    // unset-`dark` default (near-black in every theme). Contrast,
                    // computed against `Theme`'s actual token values and stated in
                    // `ui/README.md` "App shell (M3.1)":
                    //   selected   window/windowText = accent/accentText   6.70:1 light, 6.55:1 dark
                    //   unselected dark/brightText   = surfaceAlt/textMuted 5.06:1 light, 5.69:1 dark
                    palette.window: Theme.tokens.accent
                    palette.windowText: Theme.tokens.accentText
                    palette.dark: Theme.tokens.surfaceAlt
                    palette.brightText: Theme.tokens.textMuted
                    palette.mid: Theme.tokens.selection

                    Repeater {
                        model: tabsModel

                        delegate: TabButton {
                            required property string label

                            text: label
                        }
                    }
                }

                ToolButton {
                    id: addButton
                    objectName: "addWorksheetTab"
                    text: "+"
                    width: 32
                    height: parent.height
                    palette.buttonText: Theme.tokens.text
                    Accessible.name: qsTr("New worksheet")
                    onClicked: worksheetArea.addTab()
                }
            }
        }

        SplitView {
            id: paneSplit
            objectName: "worksheetPaneSplit"
            orientation: Qt.Vertical
            width: parent.width
            height: parent.height - tabBarRow.height

            handle: Rectangle {
                implicitWidth: 4
                implicitHeight: 4
                color: SplitHandle.pressed ? Theme.tokens.accent : Theme.tokens.border
            }

            Rectangle {
                id: editorPane
                objectName: "editorArea"
                SplitView.fillHeight: true
                SplitView.minimumHeight: 60
                color: Theme.tokens.editorBackground

                Accessible.role: Accessible.EditableText
                Accessible.name: qsTr("SQL editor")

                Text {
                    anchors.centerIn: parent
                    text: qsTr("SQL editor placeholder — M4.1")
                    color: Theme.tokens.textMuted
                    font.pixelSize: 13
                }
            }

            Rectangle {
                id: resultPane
                objectName: "resultArea"
                SplitView.preferredHeight: 200
                SplitView.minimumHeight: 60
                color: Theme.tokens.surface

                Accessible.role: Accessible.Pane
                Accessible.name: qsTr("Result grid")

                Text {
                    anchors.centerIn: parent
                    text: qsTr("Result grid placeholder — M4.x")
                    color: Theme.tokens.textMuted
                    font.pixelSize: 13
                }
            }
        }
    }
}
