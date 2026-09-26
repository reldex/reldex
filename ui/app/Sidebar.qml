import QtQuick
import QtQuick.Controls

// Left sidebar (M3.1): a placeholder object-browser tree (M6.1 replaces the
// model with the real `MetadataProvider`-backed one) plus a connections
// section (M3.2: the real `ProfileModel`-backed list, and the entry point
// into `ConnectionManagerDialog`). Presentation only -- ARCHITECTURE.md
// invariant 4: every list row, environment label and production badge below
// comes straight off `ConnectionManager`'s model; this file only lays it out.
Rectangle {
    id: sidebar
    objectName: "sidebar"

    // M3.2: `Main.qml` passes `bridge.connections`. `var`, not a typed
    // `ConnectionManager`, so this file (and a test that loads it standalone)
    // does not need to import `Reldex.Adapter` just to name the type.
    property var connectionManager: null

    color: Theme.tokens.surface
    border.color: Theme.tokens.border
    border.width: 1

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Sidebar")

    ConnectionManagerDialog {
        id: connectionManagerDialog
        connectionManager: sidebar.connectionManager
    }

    ListModel {
        id: objectBrowserModel

        // Placeholder rows for the object browser (M6.1); indentation is
        // baked into `indent` rather than built from a real hierarchy --
        // building a real tree here would pre-empt M6.1's
        // MetadataProvider-backed model (AGENTS.md "Scope discipline").
        // One row carries a fixed Thai sample string
        // (docs/exec-plans/active/phase-1.md row M3.1's Thai-rendering
        // check): it is not translated text, it exists only to prove the
        // font can shape Thai and the label is not clipped or zero-width.
        ListElement { label: "▾ Schemas"; indent: 0; thaiSample: false }
        ListElement { label: "▸ HR"; indent: 1; thaiSample: false }
        ListElement { label: "▸ SALES"; indent: 1; thaiSample: false }
        ListElement { label: "▾ ตัวอย่าง (ฐานข้อมูล)"; indent: 1; thaiSample: true }
        ListElement { label: "Tables"; indent: 2; thaiSample: false }
        ListElement { label: "Views"; indent: 2; thaiSample: false }
    }

    Column {
        anchors.fill: parent
        anchors.margins: 6
        spacing: 6

        Text {
            text: qsTr("OBJECT BROWSER")
            font.pixelSize: 11
            font.bold: true
            color: Theme.tokens.textMuted
        }

        ListView {
            id: tree
            objectName: "sidebarTree"
            width: parent.width
            height: 190
            clip: true
            model: objectBrowserModel

            Accessible.role: Accessible.List
            Accessible.name: qsTr("Object browser")

            delegate: Item {
                id: rowDelegate

                required property string label
                required property int indent
                required property bool thaiSample

                width: tree.width
                height: 22

                Text {
                    objectName: rowDelegate.thaiSample ? "thaiSampleSidebar" : ""
                    x: 6 + rowDelegate.indent * 12
                    anchors.verticalCenter: parent.verticalCenter
                    text: rowDelegate.label
                    color: Theme.tokens.text
                    font.pixelSize: 12
                }
            }
        }

        Rectangle {
            width: parent.width
            height: 1
            color: Theme.tokens.border
        }

        Text {
            text: qsTr("CONNECTIONS")
            font.pixelSize: 11
            font.bold: true
            color: Theme.tokens.textMuted
        }

        Text {
            objectName: "connectionsPlaceholder"
            // `ListView.count`, not the model's `rowCount()`: the latter is a
            // plain C++ method with no `NOTIFY`, so a QML binding on it would
            // never re-evaluate as profiles are added or removed. `count` is
            // the view's own reactive property.
            visible: connectionsList.count === 0
            text: qsTr("No connections configured yet.")
            color: Theme.tokens.textMuted
            font.pixelSize: 12
            font.italic: true
            wrapMode: Text.WordWrap
            width: parent.width
        }

        ListView {
            id: connectionsList
            objectName: "connectionsList"
            visible: count > 0
            width: parent.width
            height: 120
            clip: true
            model: sidebar.connectionManager ? sidebar.connectionManager.profiles : null

            Accessible.role: Accessible.List
            Accessible.name: qsTr("Connections")

            delegate: ItemDelegate {
                id: connectionDelegate

                required property int index
                required property string name
                required property bool treatAsProduction
                required property string endpointSummary

                width: connectionsList.width
                Accessible.name: name
                onClicked: {
                    connectionManagerDialog.loadRow(index)
                    connectionManagerDialog.open()
                }

                contentItem: Column {
                    Row {
                        spacing: 4
                        Text {
                            text: connectionDelegate.name
                            color: Theme.tokens.text
                            font.pixelSize: 12
                        }
                        Text {
                            visible: connectionDelegate.treatAsProduction
                            text: qsTr("PROD")
                            color: Theme.tokens.error
                            font.bold: true
                            font.pixelSize: 9
                        }
                    }
                    Text {
                        text: connectionDelegate.endpointSummary
                        color: Theme.tokens.textMuted
                        font.pixelSize: 10
                        elide: Text.ElideRight
                        width: connectionsList.width
                    }
                }
            }
        }

        Button {
            objectName: "manageConnectionsButton"
            text: qsTr("Manage Connections...")
            width: parent.width
            enabled: sidebar.connectionManager !== null
            Accessible.name: qsTr("Manage connections")
            onClicked: {
                connectionManagerDialog.resetDraftForNew()
                connectionManagerDialog.open()
            }
        }
    }
}
