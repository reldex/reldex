import QtQuick

// Persistent production indicator (M3.4, SPEC.md §17: "Production should
// have a persistent visual indicator"). Icon + text, never colour alone
// (docs/exec-plans/active/phase-1.md row M3.4's acceptance criterion) --
// `Theme.tokens.warning` colours both the glyph and the label, but the
// glyph's shape and the label's text are what actually carry the meaning,
// so the indicator survives a greyscale/colour-blind render unchanged
// (`ui/tests/tst_coreinfo.cpp`'s `productionIndicatorPassesGreyscaleCheck()`
// asserts exactly that, rather than a colour comparison).
//
// Presentation only, per ARCHITECTURE.md invariant 4: `active` is expected
// to be bound to `SessionController.activeProfileIsProduction`, which is
// itself `Profile::treat_as_production()` read through the adapter
// (ADR-0006 P3) -- this file never looks at `ReldexEnvironmentKind` and
// makes no decision of its own about what counts as "production".
//
// Reused at every place a statement can be run today (worksheet header,
// worksheet tab badge, status bar) via `compact` for the smaller tab-badge
// form; see `ui/README.md` "Production indicator (M3.4)" for the full list
// of call sites and why each one binds to the same `active` source.
Row {
    id: root

    property bool active: false
    property bool compact: false

    visible: root.active
    spacing: 3

    // Non-interactive: nothing here takes keyboard focus, so a Name is the
    // whole of this control's accessibility surface (matching the
    // StaticText role already used for informational, focus-free content
    // elsewhere in this shell).
    Accessible.role: Accessible.StaticText
    Accessible.name: qsTr("Production environment")

    Text {
        objectName: "productionIndicatorIcon"
        // A Unicode glyph, not a raster asset ("SVG or font-based, no raster
        // PNGs" -- see ui/README.md "High-DPI"): a warning triangle is a
        // shape distinct from any neutral badge before colour ever enters
        // into it, which is what keeps this legible in greyscale.
        text: "⚠"
        color: Theme.tokens.warning
        font.pixelSize: root.compact ? 10 : 13
        font.bold: true
    }

    Text {
        objectName: "productionIndicatorLabel"
        text: root.compact ? qsTr("PROD") : qsTr("PRODUCTION")
        color: Theme.tokens.warning
        font.bold: true
        font.pixelSize: root.compact ? 9 : 12
    }
}
