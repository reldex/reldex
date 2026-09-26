import QtQuick
import QtQuick.Controls

import Reldex.Adapter

// M3.3: asked before disconnecting while a transaction may be open. Reldex
// never commits or rolls back on the user's behalf (SPEC.md §10): the core
// refuses a close that names no decision, and this dialog is where the user
// names one. "Keep connected" leaves everything as it was.
//
// Presentation only: `SessionController.transactionPossiblyActive` is the
// adapter's answer (conservative -- after any statement but DDL the thin
// driver cannot rule a transaction out), and `disconnectSession()` does the
// rest.
Popup {
    id: root
    objectName: "disconnectConfirmDialog"

    parent: Overlay.overlay
    modal: true
    focus: true
    closePolicy: Popup.CloseOnEscape

    width: Math.min(480, parent ? parent.width - 40 : 480)
    x: parent ? (parent.width - width) / 2 : 0
    y: parent ? (parent.height - height) / 3 : 0

    required property var session

    background: Rectangle {
        color: Theme.tokens.surface
        border.color: Theme.tokens.border
        border.width: 1
        radius: 4
    }

    function decide(choice) {
        root.close()
        if (root.session) {
            root.session.disconnectSession(choice)
        }
    }

    contentItem: Column {
        spacing: 10
        padding: 6

        // On the content, not the Popup: a Popup is not an Item, so it cannot
        // carry the Accessible attached property itself.
        Accessible.role: Accessible.Dialog
        Accessible.name: qsTr("Disconnect")

        Text {
            text: qsTr("A transaction may be open")
            color: Theme.tokens.text
            font.pixelSize: 15
            font.bold: true
        }

        Text {
            width: root.availableWidth - 12
            text: qsTr("Disconnecting ends the session. Commit or roll back first; nothing is decided for you.")
            color: Theme.tokens.textMuted
            font.pixelSize: 12
            wrapMode: Text.WordWrap
        }

        Row {
            spacing: 8
            x: root.availableWidth - width - 12

            Button {
                objectName: "disconnectKeepConnected"
                text: qsTr("Keep connected")
                onClicked: root.close()
            }

            Button {
                objectName: "disconnectRollback"
                text: qsTr("Roll back and disconnect")
                onClicked: root.decide(SessionController.RollbackAndDisconnect)
            }

            Button {
                objectName: "disconnectCommit"
                text: qsTr("Commit and disconnect")
                onClicked: root.decide(SessionController.CommitAndDisconnect)
            }
        }
    }
}
