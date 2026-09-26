import QtQuick
import QtQuick.Controls

import Reldex.Adapter

// Bottom output panes (M3.1): a tab bar over Messages and DBMS_OUTPUT.
// Collapsible as a whole via Main.qml's Ctrl+J shortcut. Presentation only --
// ARCHITECTURE.md invariant 4: every value the DBMS_OUTPUT tab shows (the
// enable/size toggle, its provenance, the truncation counts, the lines
// themselves) comes from `bridge.serverOutput` (`ServerOutputController`,
// M4.7) -- this file only lays it out and maps a `ReldexSettingLevel` int to
// display text.
//
// M4.7 hand-off: `bridge.serverOutput` is "the active worksheet's pane", the
// same maturity level as `bridge.session` as of M3.4 -- see
// `ui/adapter/ServerOutputController.h`'s own documentation for what wiring
// one instance per worksheet tab (M4.9) will look like.
Rectangle {
    id: outputPanes
    objectName: "outputPanes"
    color: Theme.tokens.surface

    /// Set by Main.qml, the same way Sidebar.qml already receives it.
    property var bridge: null
    readonly property var serverOutput: bridge ? bridge.serverOutput : null

    Accessible.role: Accessible.Pane
    Accessible.name: qsTr("Output panes")

    /// A `ServerOutputController.SettingLevel` (== `ReldexSettingLevel`) as
    /// short display text. Presentation only -- a label for an already-
    /// resolved value, not a decision about which value is in force.
    function levelLabel(level) {
        switch (level) {
        case ServerOutputController.LevelWorksheet: return qsTr("this worksheet");
        case ServerOutputController.LevelProfile: return qsTr("connection profile");
        case ServerOutputController.LevelApplication: return qsTr("application default");
        case ServerOutputController.LevelBuiltIn: return qsTr("built-in default");
        default: return qsTr("unknown");
        }
    }

    Column {
        anchors.fill: parent

        TabBar {
            id: outputTabBar
            objectName: "outputTabBar"
            width: parent.width

            // See WorksheetArea.qml's tabBar for why these four roles
            // specifically (Basic's TabButton.qml reads them, not
            // button/buttonText/highlight) and the computed contrast ratios
            // (also in ui/README.md "App shell (M3.1)").
            palette.window: Theme.tokens.accent
            palette.windowText: Theme.tokens.accentText
            palette.dark: Theme.tokens.surfaceAlt
            palette.brightText: Theme.tokens.textMuted
            palette.mid: Theme.tokens.selection

            TabButton { text: qsTr("Messages") }
            TabButton { objectName: "dbmsOutputTabButton"; text: qsTr("DBMS_OUTPUT") }

            // Lazy open (M6.1's "open on first expand" precedent): nothing
            // about this pane's session or settings is touched until the
            // user actually looks at it -- server output costs a round trip
            // per statement once on, and the pane is off by default for
            // exactly that reason (phase-1.md row M4.7).
            onCurrentIndexChanged: {
                if (currentIndex === 1 && outputPanes.serverOutput) {
                    outputPanes.serverOutput.open()
                }
            }
        }

        Item {
            width: parent.width
            height: parent.height - outputTabBar.height

            Text {
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.margins: 8
                visible: outputTabBar.currentIndex === 0
                text: qsTr("No messages yet.")
                color: Theme.tokens.textMuted
                font.pixelSize: 12
            }

            // -------------------------------------------------- DBMS_OUTPUT
            //
            // An Item with two anchored children (the header column, the
            // list) rather than a single Column with manual height
            // arithmetic -- no QtQuick.Layouts (AGENTS.md), and anchoring
            // the list's top to the header's bottom tracks the header's real
            // height (which changes: the truncation notice below appears
            // and disappears) without recomputing anything by hand.
            Item {
                id: dbmsOutputTab
                objectName: "dbmsOutputPane"
                visible: outputTabBar.currentIndex === 1
                width: parent.width
                height: parent.height

                Accessible.role: Accessible.Pane
                Accessible.name: qsTr("DBMS_OUTPUT")

                Column {
                    id: dbmsOutputHeader
                    objectName: "dbmsOutputHeader"
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    spacing: 4

                    Row {
                        id: controlsRow
                        width: parent.width
                        height: 26
                        spacing: 10
                        leftPadding: 8
                        rightPadding: 8

                        CheckBox {
                            id: enabledCheckBox
                            objectName: "serverOutputEnabledCheckBox"
                            text: qsTr("Enable")
                            checked: outputPanes.serverOutput ? outputPanes.serverOutput.enabled : false
                            enabled: outputPanes.serverOutput ? outputPanes.serverOutput.ready : false
                            Accessible.name: qsTr("Enable DBMS_OUTPUT")
                            onToggled: outputPanes.serverOutput.setEnabled(checked)
                        }

                        Text {
                            objectName: "serverOutputEnabledProvenance"
                            anchors.verticalCenter: parent.verticalCenter
                            visible: outputPanes.serverOutput != null
                            text: outputPanes.serverOutput
                                  ? qsTr("(%1)").arg(outputPanes.levelLabel(
                                                          outputPanes.serverOutput.enabledLevel))
                                  : ""
                            color: Theme.tokens.textMuted
                            font.pixelSize: 10
                        }

                        CheckBox {
                            id: unlimitedCheckBox
                            objectName: "serverOutputUnlimitedCheckBox"
                            text: qsTr("Unlimited")
                            checked: outputPanes.serverOutput ? outputPanes.serverOutput.unlimited : false
                            enabled: outputPanes.serverOutput ? outputPanes.serverOutput.ready : false
                            Accessible.name: qsTr("Unlimited DBMS_OUTPUT buffer")
                            onToggled: {
                                if (checked) {
                                    outputPanes.serverOutput.setUnlimited()
                                } else {
                                    outputPanes.serverOutput.setBufferBytes(
                                                bufferField.text ? parseInt(bufferField.text) : 0)
                                }
                            }
                        }

                        TextField {
                            id: bufferField
                            objectName: "serverOutputBufferField"
                            width: 90
                            enabled: outputPanes.serverOutput
                                     ? (outputPanes.serverOutput.ready && !unlimitedCheckBox.checked)
                                     : false
                            text: outputPanes.serverOutput
                                  ? String(outputPanes.serverOutput.bufferBytes) : ""
                            validator: IntValidator {
                                bottom: outputPanes.serverOutput
                                        ? outputPanes.serverOutput.minBufferBytes : 0
                                top: outputPanes.serverOutput
                                     ? outputPanes.serverOutput.maxBufferBytes : 0
                            }
                            Accessible.name: qsTr("DBMS_OUTPUT buffer size in bytes")
                            onEditingFinished: outputPanes.serverOutput.setBufferBytes(parseInt(text))
                        }

                        Text {
                            objectName: "serverOutputBufferProvenance"
                            anchors.verticalCenter: parent.verticalCenter
                            visible: outputPanes.serverOutput != null
                            text: outputPanes.serverOutput
                                  ? qsTr("(%1)").arg(outputPanes.levelLabel(
                                                          outputPanes.serverOutput.bufferLevel))
                                  : ""
                            color: Theme.tokens.textMuted
                            font.pixelSize: 10
                        }

                        Button {
                            id: clearButton
                            objectName: "serverOutputClearButton"
                            text: qsTr("Clear")
                            Accessible.name: qsTr("Clear DBMS_OUTPUT")
                            onClicked: outputPanes.serverOutput.clear()
                        }

                        // Interim stand-in for a worksheet's own execution
                        // session (M4.1's editor is a placeholder, M4.9's
                        // per-tab session does not exist yet) -- see
                        // ServerOutputController::runSampleStatement()'s own
                        // doc comment.
                        Button {
                            id: runSampleButton
                            objectName: "serverOutputRunSampleButton"
                            text: qsTr("Run sample PL/SQL")
                            enabled: outputPanes.serverOutput ? outputPanes.serverOutput.ready : false
                            Accessible.name: qsTr("Run sample PL/SQL block")
                            onClicked: outputPanes.serverOutput.runSampleStatement()
                        }
                    }

                    // The override warning (phase-1.md row M4.7, verbatim):
                    // the user's own DBMS_OUTPUT calls always win over this
                    // pane's own setting.
                    Text {
                        objectName: "serverOutputOverrideNotice"
                        width: parent.width - 16
                        leftPadding: 8
                        rightPadding: 8
                        wrapMode: Text.WordWrap
                        text: qsTr("Your own DBMS_OUTPUT calls override this pane: an ENABLE(2000) "
                                   + "in your code overflows at 2,000 bytes even under an "
                                   + "“Unlimited” pane, and a DISABLE in your code stops "
                                   + "output until the next statement re-enables it.")
                        color: Theme.tokens.textMuted
                        font.pixelSize: 10
                        font.italic: true
                    }

                    // Truncation/invalid-UTF-8/read-failure notice -- never
                    // silent (phase-1.md row M4.7).
                    Text {
                        objectName: "serverOutputTruncationNotice"
                        visible: outputPanes.serverOutput
                                 ? (outputPanes.serverOutput.droppedLines > 0
                                    || outputPanes.serverOutput.invalidUtf8Lines > 0
                                    || outputPanes.serverOutput.readFailed)
                                 : false
                        width: parent.width - 16
                        leftPadding: 8
                        rightPadding: 8
                        wrapMode: Text.WordWrap
                        color: outputPanes.serverOutput && outputPanes.serverOutput.readFailed
                               ? Theme.tokens.error : Theme.tokens.warning
                        font.pixelSize: 11
                        text: {
                            if (!outputPanes.serverOutput) {
                                return ""
                            }
                            const parts = []
                            if (outputPanes.serverOutput.droppedLines > 0) {
                                parts.push(qsTr("%1 line(s) dropped (buffer cap %2 bytes exceeded)")
                                           .arg(outputPanes.serverOutput.droppedLines)
                                           .arg(outputPanes.serverOutput.unlimited
                                                ? qsTr("unlimited")
                                                : outputPanes.serverOutput.bufferBytes))
                            }
                            if (outputPanes.serverOutput.invalidUtf8Lines > 0) {
                                parts.push(qsTr("%1 line(s) received as invalid UTF-8")
                                           .arg(outputPanes.serverOutput.invalidUtf8Lines))
                            }
                            if (outputPanes.serverOutput.readFailed) {
                                parts.push(qsTr("Output incomplete: %1")
                                           .arg(outputPanes.serverOutput.readFailureMessage))
                            }
                            return parts.join(qsTr("  •  "))
                        }
                    }

                    Text {
                        objectName: "serverOutputEmptyHint"
                        leftPadding: 8
                        // `serverOutputList.count`, not the model's own
                        // `rowCount()` -- see Sidebar.qml's own comment on
                        // the same distinction: `count` is the view's
                        // reactive property, a bare C++ method has no
                        // `NOTIFY`.
                        visible: serverOutputList.count === 0
                        text: qsTr("No DBMS_OUTPUT yet.")
                        color: Theme.tokens.textMuted
                        font.pixelSize: 12
                    }
                } // dbmsOutputHeader

                ListView {
                    id: serverOutputList
                    objectName: "serverOutputList"
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: dbmsOutputHeader.bottom
                    anchors.bottom: parent.bottom
                    clip: true
                    // Virtualization (ARCHITECTURE.md "never one QML object
                    // per row"): delegates are reused rather than
                    // instantiated per line, and only a bounded window past
                    // the viewport is kept alive at all -- the model
                    // (`ServerOutputModel`) can hold far more lines than
                    // this ever instantiates (measured with 100,000 lines,
                    // ui/README.md "DBMS_OUTPUT pane (M4.7)").
                    reuseItems: true
                    cacheBuffer: 400
                    model: outputPanes.serverOutput ? outputPanes.serverOutput.model : null

                    Accessible.role: Accessible.List
                    Accessible.name: qsTr("DBMS_OUTPUT lines")

                    delegate: Text {
                        required property string lineText
                        width: ListView.view.width
                        text: lineText
                        color: Theme.tokens.text
                        font.pixelSize: 11
                        wrapMode: Text.NoWrap
                    }
                }
            } // dbmsOutputTab
        }
    }
}
