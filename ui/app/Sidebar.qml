import QtQuick

// Left sidebar (M3.1): a placeholder object-browser tree (M6.1 replaces the
// model with the real `MetadataProvider`-backed one) plus a connections
// section (M3.2 replaces the placeholder text with the real
// `ProfileModel`-backed list). Presentation only -- ARCHITECTURE.md
// invariant 4.
Rectangle {
    id: sidebar
    objectName: "sidebar"

    color: Theme.tokens.surface
    border.color: Theme.tokens.border
    border.width: 1

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Sidebar")

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
            // M3.2 replaces this with the real ProfileModel-backed list.
            objectName: "connectionsPlaceholder"
            text: qsTr("No connections configured yet.")
            color: Theme.tokens.textMuted
            font.pixelSize: 12
            font.italic: true
            wrapMode: Text.WordWrap
            width: parent.width
        }
    }
}
