import QtQuick
import QtQuick.Controls

// Centre worksheet area (M3.1): a tab bar over worksheet tabs, each tab's
// content a draggable vertical split between an editor placeholder (M4.1
// builds the real SQL/PL-SQL editor) and a result-grid placeholder (M4.x).
// Tab add/remove here is pure UI state -- no session is opened, no
// `SessionController` exists yet (that is M3.3/M4.9's "N sessions, per-tab
// state" work). Presentation only -- ARCHITECTURE.md invariant 4.
//
// M3.4 adds two of the three places SPEC.md §17's persistent production
// indicator must appear: a worksheet-header strip above the editor/result
// split, and a badge on each worksheet's own tab (the third is
// `StatusBar.qml`). Every worksheet here still shares the one
// `SessionController`/session `Bridge` owns (M4.9 is what gives each tab its
// own), so `productionActive` -- set by `Main.qml` from
// `SessionController.activeProfileIsProduction` -- applies to all of them
// alike; see `ui/README.md` "Production indicator (M3.4)".
Rectangle {
    id: worksheetArea
    objectName: "worksheetArea"
    color: Theme.tokens.background

    /// M3.4: whether the worksheet's active profile is production.
    property bool productionActive: false

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
                            id: tabButton
                            required property string label

                            text: label

                            // M3.4 tab badge: overlaid rather than folded into
                            // `text`, so the icon/label pair keeps its own
                            // colour and size independent of Basic's
                            // TabButton palette roles (documented above).
                            ProductionIndicator {
                                objectName: "productionIndicatorTabBadge"
                                compact: true
                                active: worksheetArea.productionActive
                                anchors.top: parent.top
                                anchors.right: parent.right
                                anchors.topMargin: 3
                                anchors.rightMargin: 4
                            }
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

        // M3.4's worksheet-header placement: a slim strip above the
        // editor/result split, present only while the active profile is
        // production (zero height otherwise, so nothing shifts for the
        // common case).
        Rectangle {
            id: worksheetHeader
            objectName: "worksheetHeader"
            width: parent.width
            height: headerIndicator.active ? 22 : 0
            clip: true
            color: Theme.tokens.surfaceAlt

            ProductionIndicator {
                id: headerIndicator
                objectName: "productionIndicatorWorksheetHeader"
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                anchors.leftMargin: 8
                active: worksheetArea.productionActive
            }
        }

        SplitView {
            id: paneSplit
            objectName: "worksheetPaneSplit"
            orientation: Qt.Vertical
            width: parent.width
            height: parent.height - tabBarRow.height - worksheetHeader.height

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
