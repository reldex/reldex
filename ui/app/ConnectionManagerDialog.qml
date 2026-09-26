import QtQuick
import QtQuick.Controls

// Connection manager dialog (M3.2, SPEC.md §17): list, create/edit/delete
// with confirm, environment picker (production flag free only for Custom,
// ADR-0006 P3), an endpoint form for Easy Connect/service, SID, or a full
// connect string, a session-role picker (Normal/SYSDBA/SYSOPER), and a
// test-connect action against the existing session hub.
//
// Presentation only (ARCHITECTURE.md invariant 4): every validation rule,
// every ADR-0007 write-order/clearing choice, and every FFI-error-to-message
// mapping lives in `ConnectionManager` (`ui/adapter/ConnectionManager.h`).
// This file only decides which control is enabled/visible for a given
// selection (e.g. the production toggle locked for a named environment) --
// the adapter is still what refuses a mismatched save, this is only the
// convenience that keeps the user from round-tripping into that refusal.
//
// What this dialog does NOT expose (M3.2 fix round, 2026-09-26 -- corrected
// after a review found the previous doc claim, "a disabled transport picker
// with a note", did not match what was actually shipped): there is no
// transport, CA-directory, or certificate-pin control at all. Every profile
// is created with `transport: Plain` and an empty `caDirectory`, silently.
// TCPS (TLS), a configurable CA and the certificate-pin/DN guard are M3.5's
// scope (Opus) -- see the short caption next to the role picker below, which
// is the "note" that previously did not exist.
//
// Plain `Row`/`Column` throughout, not `QtQuick.Layouts`: that module is not
// among `ui/CMakeLists.txt`'s Qt components, and every other file in this
// shell already lays itself out with anchors/explicit widths -- adding a new
// Qt module for one dialog is not worth the risk of it being unavailable on
// a CI runner's Qt install.
Popup {
    id: root
    objectName: "connectionManagerDialog"

    // A bug the M3.2 fix round's new DPI test caught (2026-09-26): without
    // this, a `Popup` declared inline (as this one is, inside Sidebar.qml)
    // takes its *declaring* item as `parent` -- the 240px-wide sidebar pane,
    // not the window -- so `width`/`x` below resolved against a 240px
    // parent instead of the whole app window, rendering the dialog roughly
    // 200px wide (unusably narrow; every field inside effectively collapsed
    // to zero width). `Overlay.overlay` is the standard Qt Quick Controls
    // attachment point for a popup that should size/center against the
    // whole window regardless of where it happens to be declared.
    parent: Overlay.overlay

    modal: true
    focus: true
    closePolicy: Popup.CloseOnEscape

    width: Math.min(820, parent ? parent.width - 40 : 820)
    height: Math.min(560, parent ? parent.height - 40 : 560)
    x: parent ? (parent.width - width) / 2 : 0
    y: parent ? (parent.height - height) / 2 : 0

    background: Rectangle {
        color: Theme.tokens.surface
        border.color: Theme.tokens.border
        border.width: 1
        radius: 4
    }

    required property var connectionManager

    // The row currently shown in the edit form, or -1 for "new profile".
    property int selectedRow: -1
    // Local, editable draft -- read from ProfileModel.get() when a row is
    // selected, or reset to defaults for "New". Never written back to the
    // model directly: only ConnectionManager.createProfile()/updateProfile()
    // do that, once the user presses Save.
    property var draft: ({})
    property bool isNew: selectedRow < 0
    property string password: ""
    property bool savePassword: false

    function resetDraftForNew() {
        selectedRow = -1
        draft = {
            id: "", name: "", environment: 1 /* Development */, environmentLabel: "",
            treatAsProduction: false, endpointKind: 1 /* HostPort */, host: "", port: 1521,
            serviceTargetKind: 1 /* ServiceName */, serviceNameOrSid: "", connectString: "",
            username: "", authKind: 1 /* Password */, passwordStorage: 2 /* PromptEachTime */,
            sessionRole: 1 /* Normal */, transport: 1 /* Plain */, caDirectory: "",
            allowUnenforcedCertificatePin: false
        }
        password = ""
        savePassword = false
        statusText.text = ""
    }

    function loadRow(row) {
        selectedRow = row
        draft = connectionManager.profiles.get(row)
        password = ""
        savePassword = draft.passwordStorage === 1 /* CredentialStore */
        statusText.text = ""
    }

    function fieldsForSave() {
        return {
            name: draft.name, environment: draft.environment,
            environmentLabel: draft.environmentLabel, treatAsProduction: draft.treatAsProduction,
            endpointKind: draft.endpointKind, host: draft.host, port: draft.port,
            serviceTargetKind: draft.serviceTargetKind, serviceNameOrSid: draft.serviceNameOrSid,
            connectString: draft.connectString, username: draft.username,
            authKind: draft.authKind,
            passwordStorage: (draft.authKind === 1 && savePassword && connectionManager.canStoreCredential)
                    ? 1 /* CredentialStore */ : 2 /* PromptEachTime */,
            password: password, sessionRole: draft.sessionRole, transport: draft.transport,
            caDirectory: draft.caDirectory,
            allowUnenforcedCertificatePin: draft.allowUnenforcedCertificatePin
        }
    }

    onOpened: resetDraftForNew()

    Connections {
        target: connectionManager
        function onProfileSaved(idHex, created) {
            statusText.color = Theme.tokens.text
            statusText.text = created ? qsTr("Profile created.") : qsTr("Profile saved.")
            const row = connectionManager.profiles.indexOfId(idHex)
            if (row >= 0) {
                root.loadRow(row)
            }
        }
        function onProfileSaveFailed(messageKey, message) {
            statusText.color = Theme.tokens.error
            statusText.text = message
        }
        function onProfileDeleted(idHex) {
            if (root.selectedRow >= 0 && root.draft.id === idHex) {
                root.resetDraftForNew()
            }
        }
        function onProfileDeleteFailed(messageKey, message) {
            statusText.color = Theme.tokens.error
            statusText.text = message
        }
        function onCredentialWarning(messageKey, message) {
            statusText.color = Theme.tokens.warning
            statusText.text = message
        }
        function onTestConnectSucceeded(idHex, summary) {
            statusText.color = Theme.tokens.text
            statusText.text = qsTr("Connected: %1").arg(summary)
        }
        function onTestConnectFailed(idHex, messageKey, message, authFailureWithStoredPassword) {
            statusText.color = Theme.tokens.error
            statusText.text = authFailureWithStoredPassword
                    ? qsTr("The stored password was refused. You will be prompted next time -- update it?")
                    : message
        }
    }

    // Enter-to-save: two shortcuts because "Return" (the main keyboard) and
    // "Enter" (the numeric keypad) are distinct Qt key sequences. Disabled
    // while the delete-confirmation Dialog is open so Enter there answers
    // that Yes/No prompt instead of reaching through to Save.
    Shortcut {
        sequence: "Return"
        enabled: root.opened && !deleteConfirm.opened
        onActivated: saveButton.clicked()
    }
    Shortcut {
        sequence: "Enter"
        enabled: root.opened && !deleteConfirm.opened
        onActivated: saveButton.clicked()
    }

    Row {
        anchors.fill: parent
        anchors.margins: 12
        spacing: 12

        // --- left: the profile list -----------------------------------
        Column {
            width: 220
            height: parent.height
            spacing: 6

            Text {
                text: qsTr("Connections")
                font.bold: true
                color: Theme.tokens.text
            }

            ListView {
                id: list
                objectName: "connectionManagerList"
                width: parent.width
                height: parent.height - 70
                clip: true
                model: root.connectionManager.profiles

                Accessible.role: Accessible.List
                Accessible.name: qsTr("Connection profiles")

                delegate: ItemDelegate {
                    id: delegateItem

                    required property int index
                    required property string name
                    required property bool treatAsProduction

                    width: list.width
                    highlighted: index === root.selectedRow
                    Accessible.name: name
                    onClicked: root.loadRow(index)

                    contentItem: Row {
                        spacing: 6
                        Text {
                            text: delegateItem.name
                            color: Theme.tokens.text
                            width: list.width - 60
                            elide: Text.ElideRight
                        }
                        Text {
                            visible: delegateItem.treatAsProduction
                            text: qsTr("PROD")
                            color: Theme.tokens.error
                            font.bold: true
                            font.pixelSize: 10
                        }
                    }
                }
            }

            Button {
                objectName: "newConnectionButton"
                text: qsTr("New")
                width: parent.width
                onClicked: root.resetDraftForNew()
            }
        }

        Rectangle { width: 1; height: parent.height; color: Theme.tokens.border }

        // --- right: the edit form ---------------------------------------
        Column {
            id: form
            // root's 12px margins (both sides) + the Row's two 12px gaps
            // (list-separator, separator-form) + the list column (220) + the
            // separator (1).
            width: root.width - 2 * 12 - 2 * 12 - 220 - 1
            height: parent.height
            spacing: 8

            Text {
                text: root.isNew ? qsTr("New connection") : qsTr("Edit connection")
                font.bold: true
                color: Theme.tokens.text
            }

            Flickable {
                width: form.width
                height: form.height - 90
                clip: true
                contentHeight: fields.height
                contentWidth: width

                Column {
                    id: fields
                    width: parent.width
                    spacing: 10

                    Column {
                        width: parent.width
                        spacing: 2
                        Text { text: qsTr("Name"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        TextField {
                            objectName: "nameField"
                            width: parent.width
                            text: root.draft.name || ""
                            Accessible.name: qsTr("Connection name")
                            onTextEdited: root.draft.name = text
                        }
                    }

                    Column {
                        width: parent.width
                        spacing: 2
                        Text { text: qsTr("Environment"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        Row {
                            width: parent.width
                            spacing: 6
                            ComboBox {
                                id: environmentCombo
                                objectName: "environmentCombo"
                                width: root.draft.environment === 6 ? parent.width - 146 : parent.width
                                Accessible.name: qsTr("Environment")
                                model: [qsTr("Development"), qsTr("Test"), qsTr("UAT"), qsTr("Staging"),
                                        qsTr("Production"), qsTr("Custom")]
                                // ReldexEnvironmentKind is 1-based (0 is "unknown").
                                currentIndex: (root.draft.environment || 1) - 1
                                onActivated: function (index) {
                                    root.draft.environment = index + 1
                                    if (root.draft.environment === 5 /* Production */) {
                                        root.draft.treatAsProduction = true
                                    } else if (root.draft.environment !== 6 /* Custom */) {
                                        root.draft.treatAsProduction = false
                                    }
                                }
                            }
                            TextField {
                                visible: root.draft.environment === 6
                                width: 140
                                placeholderText: qsTr("Label")
                                text: root.draft.environmentLabel || ""
                                Accessible.name: qsTr("Custom environment label")
                                onTextEdited: root.draft.environmentLabel = text
                            }
                        }
                        CheckBox {
                            objectName: "productionCheckBox"
                            text: qsTr("Treat as production")
                            // ADR-0006 P3: always on for Production, always
                            // off for every other named environment, free
                            // only for Custom.
                            checked: root.draft.treatAsProduction || false
                            enabled: root.draft.environment === 6 /* Custom */
                            Accessible.name: qsTr("Treat as production")
                            onToggled: root.draft.treatAsProduction = checked
                        }
                    }

                    Column {
                        width: parent.width
                        spacing: 4
                        Text { text: qsTr("Endpoint"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        Row {
                            spacing: 12
                            ButtonGroup { id: endpointGroup }
                            RadioButton {
                                text: qsTr("Host / port")
                                ButtonGroup.group: endpointGroup
                                checked: root.draft.endpointKind !== 2
                                onToggled: if (checked) root.draft.endpointKind = 1
                            }
                            RadioButton {
                                text: qsTr("Connect string")
                                ButtonGroup.group: endpointGroup
                                checked: root.draft.endpointKind === 2
                                onToggled: if (checked) root.draft.endpointKind = 2
                            }
                        }

                        // Host/port + service-or-SID form.
                        Row {
                            visible: root.draft.endpointKind !== 2
                            width: parent.width
                            spacing: 8
                            TextField {
                                width: parent.width - 96
                                placeholderText: qsTr("host")
                                text: root.draft.host || ""
                                Accessible.name: qsTr("Host")
                                onTextEdited: root.draft.host = text
                            }
                            TextField {
                                width: 88
                                placeholderText: qsTr("port")
                                text: String(root.draft.port || "")
                                validator: IntValidator { bottom: 1; top: 65535 }
                                Accessible.name: qsTr("Port")
                                onTextEdited: root.draft.port = parseInt(text || "0", 10)
                            }
                        }
                        Row {
                            visible: root.draft.endpointKind !== 2
                            width: parent.width
                            spacing: 8
                            ButtonGroup { id: targetGroup }
                            RadioButton {
                                text: qsTr("Service")
                                ButtonGroup.group: targetGroup
                                checked: root.draft.serviceTargetKind !== 2
                                onToggled: if (checked) root.draft.serviceTargetKind = 1
                            }
                            RadioButton {
                                text: qsTr("SID")
                                ButtonGroup.group: targetGroup
                                checked: root.draft.serviceTargetKind === 2
                                onToggled: if (checked) root.draft.serviceTargetKind = 2
                            }
                            TextField {
                                width: parent.width - 180
                                text: root.draft.serviceNameOrSid || ""
                                Accessible.name: qsTr("Service name or SID")
                                onTextEdited: root.draft.serviceNameOrSid = text
                            }
                        }

                        // Connect-string form (a full descriptor, or an Easy
                        // Connect string typed by hand -- both are free text
                        // to the core; SPEC.md §17 draws no separate shape
                        // for the latter).
                        TextField {
                            visible: root.draft.endpointKind === 2
                            width: parent.width
                            placeholderText: qsTr("Easy Connect string or full descriptor")
                            text: root.draft.connectString || ""
                            Accessible.name: qsTr("Connect string")
                            onTextEdited: root.draft.connectString = text
                        }
                    }

                    Column {
                        width: parent.width
                        spacing: 2
                        Text { text: qsTr("Username"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        TextField {
                            width: parent.width
                            text: root.draft.username || ""
                            Accessible.name: qsTr("Username")
                            onTextEdited: root.draft.username = text
                        }
                    }

                    Column {
                        width: parent.width
                        spacing: 2
                        Text { text: qsTr("Password"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        TextField {
                            objectName: "passwordField"
                            width: parent.width
                            echoMode: TextInput.Password
                            text: root.password
                            Accessible.name: qsTr("Password")
                            onTextEdited: root.password = text
                        }
                        Text {
                            text: qsTr("Used only to test-connect, or to save to the credential "
                                       + "store below -- never stored in the profile itself.")
                            color: Theme.tokens.textMuted
                            font.pixelSize: 10
                            wrapMode: Text.WordWrap
                            width: parent.width
                        }
                        CheckBox {
                            objectName: "savePasswordCheckBox"
                            text: root.connectionManager.canStoreCredential
                                    ? qsTr("Save password to the credential store")
                                    : qsTr("Prompt for the password every time (no credential "
                                           + "store on this system)")
                            checked: root.savePassword && root.connectionManager.canStoreCredential
                            enabled: root.connectionManager.canStoreCredential
                            Accessible.name: qsTr("Save password")
                            onToggled: root.savePassword = checked
                        }
                    }

                    Column {
                        width: parent.width
                        spacing: 2
                        Text { text: qsTr("Role"); color: Theme.tokens.textMuted; font.pixelSize: 11 }
                        ComboBox {
                            id: roleCombo
                            objectName: "roleCombo"
                            width: parent.width
                            Accessible.name: qsTr("Session role")
                            model: [qsTr("Normal"), qsTr("SYSDBA"), qsTr("SYSOPER")]
                            // ReldexSessionRoleKind is 1-based (0 is "unknown").
                            currentIndex: (root.draft.sessionRole || 1) - 1
                            onActivated: function (index) { root.draft.sessionRole = index + 1 }
                        }
                        Text {
                            text: qsTr("Connections use a plain, unencrypted transport. TCPS "
                                       + "(TLS), a configurable certificate authority and "
                                       + "certificate pinning are not yet available here.")
                            color: Theme.tokens.textMuted
                            font.pixelSize: 10
                            wrapMode: Text.WordWrap
                            width: parent.width
                        }
                    }
                }
            }

            Text {
                id: statusText
                width: parent.width
                wrapMode: Text.WordWrap
                color: Theme.tokens.text
            }

            Row {
                width: parent.width
                spacing: 8
                layoutDirection: Qt.LeftToRight

                Button {
                    objectName: "testConnectButton"
                    text: root.connectionManager.testConnectBusy ? qsTr("Testing...") : qsTr("Test Connect")
                    enabled: !root.isNew && !root.connectionManager.testConnectBusy
                    onClicked: {
                        root.connectionManager.testConnect(root.draft.id, root.password)
                        // The adapter has already copied whatever password
                        // was typed into its own request by the time this
                        // call returns (it is never retained past that) --
                        // nothing is served by keeping it in this dialog's
                        // own state a moment longer than needed.
                        root.password = ""
                    }
                }
                Button {
                    objectName: "deleteConnectionButton"
                    text: qsTr("Delete")
                    visible: !root.isNew
                    onClicked: deleteConfirm.open()
                }
                Button {
                    id: saveButton
                    objectName: "saveConnectionButton"
                    text: qsTr("Save")
                    highlighted: true
                    onClicked: {
                        if (root.isNew) {
                            root.connectionManager.createProfile(root.fieldsForSave())
                        } else {
                            root.connectionManager.updateProfile(root.draft.id, root.fieldsForSave())
                        }
                        // Same as Test Connect above: fieldsForSave() has
                        // already read it into the outgoing request.
                        root.password = ""
                    }
                }
                Button {
                    text: qsTr("Close")
                    onClicked: root.close()
                }
            }
        }
    }

    Dialog {
        id: deleteConfirm
        objectName: "deleteConfirmDialog"
        anchors.centerIn: parent
        // An explicit width (M3.3): without one, Basic's Dialog derives its
        // implicitWidth from the button box and the content, which reported
        // a binding loop every time a saved profile reloaded `draft` into the
        // text below -- first seen once the shell tests saved a profile.
        width: 420
        modal: true
        title: qsTr("Delete connection?")
        standardButtons: Dialog.Yes | Dialog.No
        Text {
            text: qsTr("Delete \"%1\"? This cannot be undone.").arg(root.draft.name || "")
            color: Theme.tokens.text
        }
        onAccepted: root.connectionManager.deleteProfile(root.draft.id)
    }
}
