import QtQuick
import QtQuick.Controls

import Reldex.Adapter

// Status bar (M3.1): connection/session-state placeholder (M3.3 wires the
// real SessionController state), a reserved slot for M3.4's persistent
// production indicator, and -- for now, the only reachable spot for it --
// the theme override picker (owner rule: every default is
// user-configurable; M3.6 gives it a real settings-UI home). Presentation
// only -- ARCHITECTURE.md invariant 4.
Rectangle {
    id: statusBar
    objectName: "statusBar"
    color: Theme.tokens.surfaceAlt

    Accessible.role: Accessible.StatusBar
    Accessible.name: qsTr("Status bar")

    Rectangle {
        width: parent.width
        height: 1
        color: Theme.tokens.border
    }

    Row {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: 8
        spacing: 12

        Text {
            // M3.3 wires the real SessionController state here.
            objectName: "sessionStatePlaceholder"
            text: qsTr("No active session")
            color: Theme.tokens.textMuted
            font.pixelSize: 12
        }

        // Reserved slot for M3.4's persistent production indicator (icon +
        // text + tab badge, never colour alone). Empty until M3.4.
        Item {
            objectName: "productionIndicatorSlot"
            width: 0
            height: parent.height
        }

        Text {
            // Fixed Thai sample string (phase-1.md row M3.1's Thai-rendering
            // check) -- not translated text, exists only to prove the font
            // can shape Thai and the label is not clipped or zero-width.
            objectName: "thaiSampleStatusBar"
            text: "ฐานข้อมูลตัวอย่าง"
            color: Theme.tokens.textMuted
            font.pixelSize: 12
        }
    }

    Row {
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.rightMargin: 8
        spacing: 6

        Text {
            text: qsTr("Theme:")
            color: Theme.tokens.textMuted
            font.pixelSize: 12
            anchors.verticalCenter: parent.verticalCenter
        }

        ComboBox {
            id: themeSelector
            objectName: "themeSelector"
            model: [qsTr("System"), qsTr("Light"), qsTr("Dark")]
            implicitHeight: parent.height - 4
            palette.window: Theme.tokens.surface
            palette.windowText: Theme.tokens.text
            palette.button: Theme.tokens.surface
            palette.buttonText: Theme.tokens.text
            Accessible.name: qsTr("Theme override")

            // A plain `currentIndex: AppSettings.themeOverride` binding
            // would be silently destroyed the first time the user picks an
            // item (QML breaks a declarative binding on the first
            // imperative write to the same property, and ComboBox writes
            // `currentIndex` itself when an item is activated). Two
            // one-way syncs instead of one two-way "binding" keeps external
            // changes (a future settings UI, M3.6; or a test) reflected
            // here indefinitely.
            Component.onCompleted: themeSelector.currentIndex = AppSettings.themeOverride

            Connections {
                target: AppSettings
                function onThemeOverrideChanged() {
                    themeSelector.currentIndex = AppSettings.themeOverride
                }
            }

            onActivated: function (index) {
                AppSettings.themeOverride = index
            }
        }
    }
}
