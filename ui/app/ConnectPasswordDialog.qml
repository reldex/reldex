import QtQuick
import QtQuick.Controls

import Reldex.Adapter

// M3.3: the password prompt of the worksheet connect flow. It opens when
// `SessionController.connectState` becomes `AwaitingPassword` and says why a
// password is needed, by name (`passwordPromptReason`, ADR-0007 S3).
//
// Presentation only (ARCHITECTURE.md invariant 4): whether to ask, whether a
// save may be offered, and when a typed password is saved are the adapter's
// decisions. The typed text is handed to `submitPassword()` straight from the
// field and the field is cleared at once; it is never copied into a property
// of this file.
Popup {
    id: root
    objectName: "connectPasswordDialog"

    // Size and centre against the window, not the declaring item (see
    // ConnectionManagerDialog.qml for the bug this avoids).
    parent: Overlay.overlay
    modal: true
    focus: true
    closePolicy: Popup.CloseOnEscape

    width: Math.min(460, parent ? parent.width - 40 : 460)
    x: parent ? (parent.width - width) / 2 : 0
    y: parent ? (parent.height - height) / 3 : 0

    /// The worksheet's `SessionController` (set by Main.qml).
    required property var session

    background: Rectangle {
        color: Theme.tokens.surface
        border.color: Theme.tokens.border
        border.width: 1
        radius: 4
    }

    function reasonText() {
        if (!root.session) {
            return ""
        }
        switch (root.session.passwordPromptReason) {
        case SessionController.PromptEachTime:
            return qsTr("This connection asks for its password at every connect.")
        case SessionController.NotStored:
            return qsTr("No password is saved for this connection.")
        case SessionController.StoreUnavailable:
            return qsTr("This system has no credential store, so the password is asked for at every connect.")
        case SessionController.StoreFailed:
            return qsTr("The saved password could not be read: %1").arg(root.session.passwordPromptDetail)
        case SessionController.StoredPasswordRefused:
            return qsTr("The database refused the saved password. It will not be tried again; enter the current one.")
        default:
            return ""
        }
    }

    function submit() {
        if (!root.session) {
            return
        }
        root.session.submitPassword(passwordField.text, saveBox.visible && saveBox.checked)
        passwordField.text = ""
    }

    Connections {
        target: root.session
        function onConnectStateChanged() {
            if (root.session.connectState === SessionController.AwaitingPassword) {
                passwordField.text = ""
                saveBox.checked = false
                root.open()
                passwordField.forceActiveFocus()
            } else if (root.opened) {
                root.close()
            }
        }
    }

    onClosed: {
        passwordField.text = ""
        // Escape, or anything else that closes the prompt while it is still
        // being asked for, is the user cancelling the connect.
        if (root.session && root.session.connectState === SessionController.AwaitingPassword) {
            root.session.cancelConnect()
        }
    }

    contentItem: Column {
        spacing: 10
        padding: 6

        // On the content, not the Popup: a Popup is not an Item, so it cannot
        // carry the Accessible attached property itself.
        Accessible.role: Accessible.Dialog
        Accessible.name: qsTr("Connection password")

        Text {
            objectName: "connectPasswordTitle"
            text: root.session ? qsTr("Connect to %1").arg(root.session.connectProfileName) : ""
            color: Theme.tokens.text
            font.pixelSize: 15
            font.bold: true
        }

        Text {
            objectName: "connectPasswordReason"
            width: root.availableWidth - 12
            text: root.reasonText()
            color: Theme.tokens.textMuted
            font.pixelSize: 12
            wrapMode: Text.WordWrap
        }

        TextField {
            id: passwordField
            objectName: "connectPasswordField"
            width: root.availableWidth - 12
            echoMode: TextInput.Password
            placeholderText: qsTr("Password")
            color: Theme.tokens.text
            placeholderTextColor: Theme.tokens.textMuted
            background: Rectangle {
                color: Theme.tokens.background
                border.color: passwordField.activeFocus ? Theme.tokens.accent : Theme.tokens.border
                border.width: 1
                radius: 3
            }
            Accessible.name: qsTr("Password")
            onAccepted: root.submit()
        }

        CheckBox {
            id: saveBox
            objectName: "connectPasswordSave"
            visible: root.session ? root.session.offerSavePassword : false
            text: root.session && root.session.passwordPromptReason === SessionController.StoredPasswordRefused
                  ? qsTr("Update the saved password after connecting")
                  : qsTr("Save the password after connecting")
            palette.windowText: Theme.tokens.text
        }

        Row {
            spacing: 8
            x: root.availableWidth - width - 12

            Button {
                objectName: "connectPasswordCancel"
                text: qsTr("Cancel")
                onClicked: root.close()
            }

            Button {
                objectName: "connectPasswordSubmit"
                text: qsTr("Connect")
                highlighted: true
                onClicked: root.submit()
            }
        }
    }
}
