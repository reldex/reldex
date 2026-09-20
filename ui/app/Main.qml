import QtQuick
import QtQuick.Window

import Reldex.Adapter

Window {
    id: root

    width: 480
    height: 240
    visible: true
    title: "Reldex"

    Column {
        anchors.centerIn: parent
        spacing: 8

        Text {
            text: "Reldex"
            font.pixelSize: 24
            font.bold: true
        }

        // M1.5 scope: prove the FFI -> adapter -> QML path, nothing more.
        // CoreInfo.abiVersion is the raw packed (major << 16 | minor) value
        // reldex_abi_version() returns.
        Text {
            text: "reldex-ffi ABI version: " + CoreInfo.abiVersion
            font.pixelSize: 14
        }
    }
}
