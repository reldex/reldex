import QtQuick

// Left sidebar (M3.1): the real object browser (M6.1, `ObjectBrowserPanel`)
// plus a connections section (M3.2 replaces the placeholder text with the
// real `ProfileModel`-backed list). Presentation only -- ARCHITECTURE.md
// invariant 4.
//
// A fixed Thai sample label used to live in this file's placeholder tree data
// (docs/exec-plans/active/phase-1.md row M3.1's Thai-rendering check: proving
// the font can shape Thai and the label is not clipped or zero-width).
// `ui/tests/tst_coreinfo.cpp`'s `sidebarAndStatusBarShapeThaiCorrectly()`
// looks it up by `objectName: "thaiSampleSidebar"` and asserts a nonzero
// `contentWidth` -- kept below as a small standalone `Text`, still visible so
// that check still exercises real glyph shaping. M6.3 (i18n baseline) owns
// giving this a permanent home once it lands.
Rectangle {
    id: sidebar
    objectName: "sidebar"

    /// The adapter `Bridge` the object browser opens its own metadata
    /// session against (set by `Main.qml`).
    property var bridge: null

    color: Theme.tokens.surface
    border.color: Theme.tokens.border
    border.width: 1

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Sidebar")

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

        ObjectBrowserPanel {
            id: objectBrowser
            objectName: "objectBrowserPanel"
            width: parent.width
            height: 260
            bridge: sidebar.bridge
        }

        Text {
            objectName: "thaiSampleSidebar"
            text: "ตัวอย่าง (ฐานข้อมูล)"
            color: Theme.tokens.textMuted
            font.pixelSize: 11
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
