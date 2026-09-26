import QtQuick
import QtQuick.Controls
import QtQml.Models

import Reldex.Adapter

// The object browser panel (M6.1): a lazy tree over `SPEC.md` §16's 9 object
// groups, a server-side filter box (debounced), a row-cap/truncation
// indicator, a refresh action, and a read-only columns pane for a selected
// table/view.
//
// Presentation only (ARCHITECTURE.md invariant 4, AGENTS.md "no business
// rules in QML"): every decision -- what to fetch, how an error is
// classified, when a result counts as truncated -- lives in
// `ObjectBrowserModel` (C++, `ui/adapter/ObjectBrowserModel.{h,cpp}`). This
// file only tells the model to expand/filter/activate a row (and, for the
// filter box, when -- debouncing keystrokes is interaction timing, not a
// business rule) and renders whatever the model reports back.
Item {
    id: root

    /// The adapter's `Bridge` this panel's model opens its own metadata
    /// session against -- supplied by `Sidebar.qml`. Never a worksheet's
    /// session (ADR-0002: a worksheet owns its own session).
    property var bridge: null

    /// The tree index last activated (tapped, or keyboard-activated) by the
    /// user. Drives both the filter box (which node a typed filter applies
    /// to) and the Refresh button, without depending on `TreeView`'s own
    /// selection machinery.
    property var activeIndex: null

    /// Shared by the pointer path (`TapHandler`, per delegate) and the
    /// keyboard path (`Keys.onReturnPressed`, on `tree` itself, M6.4 minimal
    /// wiring): both must produce the same result -- the row becomes the
    /// active one, the filter field reflects its own filter, and the model
    /// is told to activate it (loads the columns pane for a table/view).
    function activateModelIndex(idx) {
        root.activeIndex = idx;
        filterField.text = browserModel.filterFor(idx);
        browserModel.activate(idx);
    }

    ObjectBrowserModel {
        id: browserModel
        objectName: "objectBrowserModel"
        bridge: root.bridge
    }

    // The header (filter row + status lines) sizes itself; the tree then
    // fills exactly what is left between it and the columns pane -- anchored,
    // not a guessed pixel constant, so nothing overlaps or wastes space
    // whether or not the columns pane happens to be visible right now.
    Column {
        id: header
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        spacing: 4

        Row {
            width: parent.width
            spacing: 4

            TextField {
                id: filterField
                objectName: "objectBrowserFilterField"
                width: parent.width - refreshButton.width - parent.spacing
                placeholderText: qsTr("Filter (server-side)")
                enabled: root.activeIndex !== null
                Accessible.name: qsTr("Filter object browser")

                onTextEdited: filterDebounce.restart()

                Timer {
                    id: filterDebounce
                    interval: 300
                    onTriggered: {
                        if (root.activeIndex !== null) {
                            browserModel.setFilter(root.activeIndex, filterField.text);
                        }
                    }
                }
            }

            Button {
                id: refreshButton
                objectName: "objectBrowserRefreshButton"
                text: qsTr("Refresh")
                enabled: root.activeIndex !== null
                Accessible.name: qsTr("Refresh selected node")
                onClicked: {
                    if (root.activeIndex !== null) {
                        browserModel.expand(root.activeIndex, true);
                    }
                }
            }
        }

        Text {
            objectName: "objectBrowserSessionStatus"
            visible: browserModel.sessionOpening
            text: qsTr("Opening metadata session…")
            color: Theme.tokens.textMuted
            font.pixelSize: 11
        }

        Text {
            text: qsTr("Row cap: %1").arg(browserModel.rowCap)
            color: Theme.tokens.textMuted
            font.pixelSize: 10
        }
    }

    TreeView {
        id: tree
        objectName: "objectBrowserTree"
        anchors.top: header.bottom
        anchors.topMargin: 4
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: columnsPane.visible ? columnsPane.top : parent.bottom
        clip: true
        model: browserModel

        // Minimal keyboard wiring (M6.1; the rest -- Narrator verification,
        // any polish -- is M6.4's, see `ui/README.md` "Object browser
        // (M6.1)"). A `selectionModel` is what turns on `TreeView`'s own
        // built-in key handling while it has focus: Up/Down move the current
        // row, Left collapses (or moves to the parent row if already
        // collapsed), Right expands (or moves to the first child if already
        // expanded), Space toggles expanded/collapsed. `focus`/
        // `activeFocusOnTab` are what let a keyboard-only user reach it at
        // all -- neither is on by default for a plain `Item`-derived view.
        focus: true
        activeFocusOnTab: true
        selectionModel: ItemSelectionModel {
            model: browserModel
        }

        Accessible.role: Accessible.Tree
        Accessible.name: qsTr("Object browser")

        delegate: TreeViewDelegate {
            id: treeDelegate
            implicitWidth: tree.width

            text: {
                let label = model.display !== undefined ? model.display : "";
                if (model.secondary) {
                    label += "  (" + model.secondary + ")";
                }
                if (model.statusText) {
                    label += "   " + model.statusText;
                }
                return label;
            }

            Accessible.name: treeDelegate.text

            TapHandler {
                acceptedButtons: Qt.LeftButton
                onTapped: {
                    root.activateModelIndex(tree.index(treeDelegate.row, treeDelegate.column));
                }
            }
        }

        onExpanded: (row, depth) => {
            browserModel.expand(tree.index(row, 0));
        }

        // Return/Enter activates the current row exactly like a tap on it
        // (loads the columns pane for a table/view) -- `selectionModel`
        // above already gives Up/Down/Left/Right/Space their built-in
        // meaning, but "activate" is this panel's own action, not `TreeView`'s.
        // Guarded on `currentIndex.row >= 0`, not `hasSelection`: arrow-key
        // navigation alone moves `currentIndex` without adding it to the
        // selection (no Shift / drag involved), so `hasSelection` stays false
        // even once there is a perfectly good row to activate.
        Keys.onReturnPressed: (event) => {
            if (tree.selectionModel.currentIndex.row >= 0) {
                root.activateModelIndex(tree.selectionModel.currentIndex);
            }
            event.accepted = true;
        }
        Keys.onEnterPressed: (event) => {
            if (tree.selectionModel.currentIndex.row >= 0) {
                root.activateModelIndex(tree.selectionModel.currentIndex);
            }
            event.accepted = true;
        }
    }

    // --- the read-only columns pane (SPEC.md §16: "columns of a selected
    // table") -- a fixed-height panel below the tree, docked at the bottom so
    // it does not need to steal space from the tree until something is
    // selected.
    Rectangle {
        id: columnsPane
        objectName: "objectBrowserColumnsPane"
        visible: root.activeIndex !== null
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        height: 92
        color: Theme.tokens.surfaceAlt
        border.color: Theme.tokens.border
        border.width: 1

        Column {
            anchors.fill: parent
            anchors.margins: 4
            spacing: 2

            Text {
                text: browserModel.columnsPaneTitle
                font.bold: true
                font.pixelSize: 11
                color: Theme.tokens.text
                elide: Text.ElideRight
                width: parent.width
            }

            Text {
                visible: browserModel.columnsPaneLoading
                text: qsTr("Loading columns…")
                color: Theme.tokens.textMuted
                font.pixelSize: 10
            }

            Text {
                visible: browserModel.columnsPaneHasError
                text: browserModel.columnsPaneError
                color: Theme.tokens.error
                font.pixelSize: 10
            }

            ListView {
                objectName: "objectBrowserColumnsList"
                visible: !browserModel.columnsPaneLoading && !browserModel.columnsPaneHasError
                width: parent.width
                height: 60
                clip: true
                model: browserModel.columnsPaneRows

                Accessible.role: Accessible.List
                Accessible.name: qsTr("Columns")

                delegate: Text {
                    width: ListView.view.width
                    text: modelData.position + "  " + modelData.name + "  " + modelData.type
                        + "  " + (modelData.nullable === "YES" ? qsTr("NULL") : qsTr("NOT NULL"))
                    color: Theme.tokens.text
                    font.pixelSize: 10
                }
            }
        }
    }
}
