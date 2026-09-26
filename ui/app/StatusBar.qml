import QtQuick
import QtQuick.Controls

import Reldex.Adapter

// Status bar (M3.1): the worksheet's connection state (M3.3: read straight
// off `SessionController`, with Cancel while connecting and Disconnect once
// connected), M3.4's persistent production indicator, and
// -- for now, the only reachable spot for it -- the theme override picker
// (owner rule: every default is user-configurable; M3.6 gives it a real
// settings-UI home). Presentation only -- ARCHITECTURE.md invariant 4:
// `productionActive` is set by `Main.qml` straight off
// `SessionController.activeProfileIsProduction` (itself
// `Profile::treat_as_production()`, ADR-0006 P3); this file makes no
// production/non-production decision of its own.
Rectangle {
    id: statusBar
    objectName: "statusBar"
    color: Theme.tokens.surfaceAlt

    /// M3.4: whether the worksheet's active profile is production. See
    /// `ui/README.md` "Production indicator (M3.4)".
    property bool productionActive: false
    /// M3.3: the worksheet's `SessionController` (set by Main.qml), or null.
    property var session: null

    /// A translated label for the adapter's error key (the table lives in
    /// the adapter; this only picks the words).
    function errorLabel(key: string): string {
        switch (key) {
        case "error.authentication": return qsTr("authentication failed")
        case "error.connection": return qsTr("could not reach the database")
        case "error.networkLost": return qsTr("the connection was lost")
        case "error.timeout": return qsTr("timed out")
        case "error.configuration": return qsTr("the connection profile is not usable")
        case "error.cancelled": return qsTr("cancelled")
        case "error.transaction": return qsTr("a transaction may be open")
        default: return qsTr("error")
        }
    }

    /// The vendor's own message when there is one (it carries the native
    /// code, e.g. ORA-01017), otherwise Reldex's.
    function errorDetail(): string {
        if (!session || !session.hasError) {
            return ""
        }
        return session.errorNativeMessage !== "" ? session.errorNativeMessage : session.errorMessage
    }

    function sessionText(): string {
        if (!session) {
            return qsTr("No active session")
        }
        const name = session.connectProfileName
        switch (session.connectState) {
        case SessionController.Preparing:
            return qsTr("Preparing to connect to %1…").arg(name)
        case SessionController.AwaitingPassword:
            return qsTr("Password needed for %1").arg(name)
        case SessionController.Connecting:
            return session.connectTimeoutSeconds > 0
                    ? qsTr("Connecting to %1… (gives up after %2 s)").arg(name).arg(session.connectTimeoutSeconds)
                    : qsTr("Connecting to %1…").arg(name)
        case SessionController.Connected:
            if (session.hasError) {
                return qsTr("Connected to %1 · %2: %3").arg(name).arg(errorLabel(session.errorKey)).arg(errorDetail())
            }
            if (session.state === SessionController.Executing || session.state === SessionController.Fetching) {
                return qsTr("Connected to %1 · running…").arg(name)
            }
            if (session.state === SessionController.ResultComplete) {
                return qsTr("Connected to %1 · %n row(s)", "", session.rowsFetched).arg(name)
            }
            return qsTr("Connected to %1").arg(name)
        case SessionController.Disconnecting:
            return qsTr("Disconnecting from %1…").arg(name)
        case SessionController.ConnectFailed:
            return qsTr("Could not connect to %1 · %2: %3").arg(name).arg(errorLabel(session.errorKey)).arg(errorDetail())
        case SessionController.TimedOut:
            return qsTr("Connecting to %1 timed out after %2 s").arg(name).arg(session.connectTimeoutSeconds)
        case SessionController.Cancelled:
            return qsTr("Connecting to %1 was cancelled").arg(name)
        case SessionController.Lost:
            return session.transactionPossiblyLost
                    ? qsTr("Connection to %1 lost; an open transaction may have been lost with it · %2").arg(name).arg(errorDetail())
                    : qsTr("Connection to %1 lost · %2").arg(name).arg(errorDetail())
        default:
            return qsTr("Not connected")
        }
    }

    Accessible.role: Accessible.StatusBar
    Accessible.name: qsTr("Status bar")

    Rectangle {
        width: parent.width
        height: 1
        color: Theme.tokens.border
    }

    DisconnectConfirmDialog {
        id: disconnectConfirm
        session: statusBar.session
    }

    Row {
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: 8
        spacing: 12

        Text {
            id: sessionStateText
            objectName: "sessionStateText"
            text: statusBar.sessionText()
            color: statusBar.session && (statusBar.session.connectState === SessionController.ConnectFailed
                                         || statusBar.session.connectState === SessionController.TimedOut
                                         || statusBar.session.connectState === SessionController.Lost)
                   ? Theme.tokens.error : Theme.tokens.textMuted
            font.pixelSize: 12
            elide: Text.ElideRight
            width: Math.min(implicitWidth, statusBar.width * 0.5)
            Accessible.role: Accessible.StaticText
            Accessible.name: text

            // The whole failure -- kind, the vendor's message with its code,
            // and the cause chain -- on hover.
            HoverHandler { id: sessionStateHover }
            ToolTip.visible: sessionStateHover.hovered && statusBar.session !== null && statusBar.session.hasError
            ToolTip.text: statusBar.session && statusBar.session.hasError
                          ? [statusBar.errorLabel(statusBar.session.errorKey),
                             statusBar.session.errorNativeMessage,
                             statusBar.session.errorMessage,
                             statusBar.session.errorCause].filter(function (part) { return part !== "" }).join("\n")
                          : ""
        }

        Button {
            objectName: "cancelConnectButton"
            visible: statusBar.session !== null && statusBar.session.canCancelConnect
            text: qsTr("Cancel")
            implicitHeight: 22
            font.pixelSize: 11
            Accessible.name: qsTr("Cancel connecting")
            onClicked: statusBar.session.cancelConnect()
        }

        Button {
            objectName: "disconnectButton"
            visible: statusBar.session !== null && statusBar.session.canDisconnect
            text: qsTr("Disconnect")
            implicitHeight: 22
            font.pixelSize: 11
            Accessible.name: qsTr("Disconnect")
            onClicked: {
                if (statusBar.session.transactionPossiblyActive) {
                    disconnectConfirm.open()
                } else {
                    statusBar.session.disconnectSession(SessionController.DisconnectOnly)
                }
            }
        }

        ProductionIndicator {
            objectName: "productionIndicatorStatusBar"
            // No vertical anchor, matching the plain `Text` siblings in this
            // `Row`: all three simply top-align, which reads as centred
            // since they are all close to the same height.
            active: statusBar.productionActive
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
