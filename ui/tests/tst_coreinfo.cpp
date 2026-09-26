#include "AdapterTestSupport.h"

#include <QAccessible>
#include <QAccessibleInterface>
#include <QDir>
#include <QGuiApplication>
#include <QImage>
#include <QQmlApplicationEngine>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QQmlError>
#include <QQuickStyle>
#include <QSignalSpy>
#include <QTest>
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

#include <AppSettings.h>
#include <ObjectBrowserModel.h>
#include <reldex.h>

// M1.5 acceptance criteria (docs/exec-plans/active/phase-1.md, row M1.5):
//   (a) reldex_abi_version() equals the header's ABI version constant.
//   (b) the QML module / Main.qml loads via QQmlApplicationEngine under
//       QT_QPA_PLATFORM=offscreen, with the root object created and no QML
//       warnings.
// M1.6 added (c): the QML surface really is bound to the real model -- a
// query runs through the loaded tree and its column headers come back
// through QML. M1.6's surface moved to Harness.qml at M3.1 (see
// ui/app/main.cpp and ui/README.md "Running the shell vs the harness"), so
// (c) now loads "Harness" explicitly rather than "Main".
//
// M3.1 adds the app-shell checks (row M3.1's acceptance: "Renders at
// 100/150/200% DPI; theme switch has no restart"): "Main" is now the real
// shell, and the tests below instantiate it offscreen, toggle the theme
// override, collapse/expand the sidebar and output pane, check the fixed
// Thai sample strings are not zero-width, and check the shell renders
// sanely at whichever QT_SCALE_FACTOR the process was started with (see
// ui/tests/CMakeLists.txt for the three DPI ctest registrations that vary
// it across processes).
class TstCoreInfo : public QObject
{
    Q_OBJECT

private slots:
    void initTestCase();

    void abiVersionMatchesHeader();
    void qmlModuleLoadsCleanly();
    void harnessSurfaceIsBoundToTheRealModel();

    void appShellLoadsCleanly();
    void appShellThemeOverrideChangesTokensWithoutRecreatingTheWindow();
    void appShellSidebarAndOutputPaneCollapseAndExpand();
    void appShellThaiSampleTextIsNotZeroWidth();
    void appShellMainRegionsHaveAccessibleNames();
    void appShellRendersAtCurrentScaleFactor();
    void connectionManagerDialogRendersAtCurrentScaleFactor();
    void objectBrowserTreeKeyboardNavigationExpandsAndActivates();

    // M3.4: persistent production indicator (SPEC.md §17; phase-1.md row
    // M3.4). See ui/README.md "Production indicator (M3.4)".
    void productionIndicatorHiddenByDefault();
    void productionIndicatorVisibleInAllThreePlacesWhenActive();
    void productionIndicatorFollowsTheFlagNotTheEnvironment();
    void productionIndicatorPassesGreyscaleCheck();
    void productionIndicatorRendersAtCurrentScaleFactor();
};

void TstCoreInfo::initTestCase()
{
    // Must run before the first QML file importing QtQuick.Controls loads
    // (ui/app/main.cpp does the same thing, for the same reason -- see its
    // comment). Without it, Qt Quick Controls falls back to a
    // platform-default style (e.g. native "Windows" on this machine), which
    // is not what Reldex.exe actually runs and which spends every test
    // querying real OS theme handles that an offscreen platform cannot open
    // (harmless "OpenThemeData() failed" qWarnings, but pure noise here).
    QQuickStyle::setStyle(QStringLiteral("Basic"));
}

void TstCoreInfo::abiVersionMatchesHeader()
{
    QCOMPARE(reldex_abi_version(), static_cast<quint32>(RELDEX_ABI_VERSION));
}

void TstCoreInfo::qmlModuleLoadsCleanly()
{
    QQmlApplicationEngine engine;

    QList<QQmlError> warnings;
    connect(&engine, &QQmlEngine::warnings, &engine,
            [&warnings](const QList<QQmlError> &reported) { warnings += reported; });

    QSignalSpy creationFailedSpy(&engine, &QQmlApplicationEngine::objectCreationFailed);

    engine.loadFromModule("Reldex.App", "Main");

    QCOMPARE(creationFailedSpy.count(), 0);

    const QList<QObject *> roots = engine.rootObjects();
    QCOMPARE(roots.size(), 1);
    QVERIFY(roots.constFirst() != nullptr);

    QString warningText;
    for (const QQmlError &warning : std::as_const(warnings)) {
        warningText += warning.toString() + QLatin1Char('\n');
    }
    QVERIFY2(warnings.isEmpty(), qPrintable(warningText));
}

void TstCoreInfo::harnessSurfaceIsBoundToTheRealModel()
{
    QQmlApplicationEngine engine;

    QList<QQmlError> warnings;
    connect(&engine, &QQmlEngine::warnings, &engine,
            [&warnings](const QList<QQmlError> &reported) { warnings += reported; });

    engine.loadFromModule("Reldex.App", "Harness");
    const QList<QObject *> roots = engine.rootObjects();
    QCOMPARE(roots.size(), 1);

    auto *bridge = roots.constFirst()->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    QVERIFY(bridge->isValid());

    SessionController *session = bridge->session();
    session->setMockRows(500);
    session->setFetchRows(100);
    QVERIFY(bridge->run());
    QVERIFY(adapter_test::spinUntil([session] {
        return session->state() == SessionController::ResultComplete
                || session->state() == SessionController::Failed;
    }));
    QCOMPARE(session->state(), SessionController::ResultComplete);
    QCOMPARE(session->model()->rowCount(), 500);
    QCOMPARE(session->model()->columnCount(), 3);

    // The header Repeater instantiated one delegate per column, and each one
    // read its text through the model's headerData() from QML -- which is the
    // part a C++-only model test cannot reach.
    // Visual children, not QObject children: a Repeater owns its delegates and
    // only re-parents them *visually* into the Row.
    auto *header = roots.constFirst()->findChild<QQuickItem *>(QStringLiteral("header"));
    QVERIFY(header != nullptr);
    QStringList headings;
    const QList<QQuickItem *> headerItems = header->childItems();
    for (const QQuickItem *child : headerItems) {
        const QVariant text = child->property("text");
        if (text.isValid()) {
            headings.append(text.toString());
        }
    }
    QCOMPARE(headings, QStringList({ QStringLiteral("ID"), QStringLiteral("NAME"),
                                     QStringLiteral("CREATED") }));

    QString warningText;
    for (const QQmlError &warning : std::as_const(warnings)) {
        warningText += warning.toString() + QLatin1Char('\n');
    }
    QVERIFY2(warnings.isEmpty(), qPrintable(warningText));
}

void TstCoreInfo::appShellLoadsCleanly()
{
    QQmlApplicationEngine engine;

    QList<QQmlError> warnings;
    connect(&engine, &QQmlEngine::warnings, &engine,
            [&warnings](const QList<QQmlError> &reported) { warnings += reported; });

    QSignalSpy creationFailedSpy(&engine, &QQmlApplicationEngine::objectCreationFailed);

    engine.loadFromModule("Reldex.App", "Main");

    QCOMPARE(creationFailedSpy.count(), 0);
    const QList<QObject *> roots = engine.rootObjects();
    QCOMPARE(roots.size(), 1);
    QVERIFY(qobject_cast<QQuickWindow *>(roots.constFirst()) != nullptr);

    QString warningText;
    for (const QQmlError &warning : std::as_const(warnings)) {
        warningText += warning.toString() + QLatin1Char('\n');
    }
    QVERIFY2(warnings.isEmpty(), qPrintable(warningText));
}

void TstCoreInfo::appShellThemeOverrideChangesTokensWithoutRecreatingTheWindow()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    const QList<QObject *> roots = engine.rootObjects();
    QCOMPARE(roots.size(), 1);

    auto *window = qobject_cast<QQuickWindow *>(roots.constFirst());
    QVERIFY(window != nullptr);

    auto *settings = engine.singletonInstance<AppSettings *>("Reldex.Adapter", "AppSettings");
    QVERIFY(settings != nullptr);

    // Three ways, per the task brief: System, Light, Dark. Light and Dark
    // resolve unconditionally (Theme.qml); System additionally depends on
    // Application.styleHints.colorScheme, which this test does not control,
    // so only Light vs Dark is asserted to actually differ.
    settings->setThemeOverride(AppSettings::Light);
    QCoreApplication::processEvents();
    const QVariant lightBackground = window->property("color");
    QVERIFY(lightBackground.isValid());

    settings->setThemeOverride(AppSettings::Dark);
    QCoreApplication::processEvents();
    const QVariant darkBackground = window->property("color");
    QVERIFY(darkBackground.isValid());

    QVERIFY2(lightBackground != darkBackground,
             "Theme.tokens.background did not change when AppSettings.themeOverride changed "
             "between Light and Dark");

    settings->setThemeOverride(AppSettings::System);
    QCoreApplication::processEvents();

    // Same window throughout: "theme switch has no restart"
    // (docs/exec-plans/active/phase-1.md row M3.1).
    QCOMPARE(qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst()), window);

    // Leave global state as found for any test that runs after this one in
    // the same process.
    settings->setThemeOverride(AppSettings::System);
}

void TstCoreInfo::appShellSidebarAndOutputPaneCollapseAndExpand()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);

    auto *sidebar = root->findChild<QQuickItem *>(QStringLiteral("sidebar"));
    auto *outputPanes = root->findChild<QQuickItem *>(QStringLiteral("outputPanes"));
    QVERIFY(sidebar != nullptr);
    QVERIFY(outputPanes != nullptr);
    QVERIFY(sidebar->isVisible());
    QVERIFY(outputPanes->isVisible());

    // Exercises the same QML functions Main.qml's Ctrl+B/Ctrl+J Shortcut
    // items call -- see that file for why a real synthetic key event is not
    // used here (offscreen window activation is not reliable enough across
    // three CI platforms to make that the automated check; it was exercised
    // interactively instead, see the M3.1 task report).
    QVERIFY(QMetaObject::invokeMethod(root, "toggleSidebar"));
    QCoreApplication::processEvents();
    QVERIFY(!sidebar->isVisible());

    QVERIFY(QMetaObject::invokeMethod(root, "toggleSidebar"));
    QCoreApplication::processEvents();
    QVERIFY(sidebar->isVisible());

    QVERIFY(QMetaObject::invokeMethod(root, "toggleOutputPane"));
    QCoreApplication::processEvents();
    QVERIFY(!outputPanes->isVisible());

    QVERIFY(QMetaObject::invokeMethod(root, "toggleOutputPane"));
    QCoreApplication::processEvents();
    QVERIFY(outputPanes->isVisible());
}

void TstCoreInfo::appShellThaiSampleTextIsNotZeroWidth()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);

    // The sidebar's ListView defers delegate instantiation to its own
    // polish/refill pass, which -- unlike a plain property binding -- only
    // runs on an actual scene-graph frame tick; a bare processEvents() can
    // return before one fires, so this needs a real wait, not just a queue
    // drain.
    QTest::qWait(50);

    // The root object is the ApplicationWindow itself (a QQuickWindow, not a
    // QQuickItem) -- its contentItem() is where the visual item tree starts.
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);
    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);

    const QStringList names = { QStringLiteral("thaiSampleSidebar"),
                                 QStringLiteral("thaiSampleStatusBar") };
    for (const QString &name : names) {
        // findVisualChild(), not findChild(): thaiSampleSidebar is a
        // ListView delegate (see that helper's comment for why
        // QObject::findChild() cannot see it).
        auto *item = adapter_test::findVisualChild(rootItem, name);
        QVERIFY2(item != nullptr, qPrintable(name));
        const QVariant contentWidth = item->property("contentWidth");
        QVERIFY2(contentWidth.isValid(), qPrintable(name));
        QVERIFY2(contentWidth.toReal() > 0.0,
                 qPrintable(name + QStringLiteral(": contentWidth is zero (clipped or tofu "
                                                   "glyphs would still report nonzero; zero "
                                                   "means the text did not shape at all)")));
    }
}

void TstCoreInfo::appShellMainRegionsHaveAccessibleNames()
{
    // Public API (QAccessible::queryAccessibleInterface), not the
    // QQuickAccessibleAttached private header: this is the same technique
    // Qt's own Quick autotests use to read a QML `Accessible.name` from C++
    // without an actual platform AT client attached.
    const bool wasActive = QAccessible::isActive();
    QAccessible::setActive(true);

    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);

    const QStringList names = { QStringLiteral("sidebar"), QStringLiteral("worksheetArea"),
                                 QStringLiteral("outputPanes"), QStringLiteral("statusBar") };
    for (const QString &name : names) {
        auto *item = root->findChild<QQuickItem *>(name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QAccessibleInterface *iface = QAccessible::queryAccessibleInterface(item);
        QVERIFY2(iface != nullptr, qPrintable(name));
        QVERIFY2(!iface->text(QAccessible::Name).isEmpty(), qPrintable(name));
    }

    QAccessible::setActive(wasActive);
}

void TstCoreInfo::appShellRendersAtCurrentScaleFactor()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);

    // SplitView defers its own layout pass (pane sizing) to the next
    // event-loop turn, same as the ListView note above.
    QCoreApplication::processEvents();
    QCoreApplication::processEvents();

    // QT_SCALE_FACTOR is read once at QPA platform start-up, so it cannot be
    // varied within one already-running process; this function is
    // registered three times in CMakeLists.txt, once per scale factor, each
    // its own ctest process (see the comment there).
    const qreal dpr = window->devicePixelRatio();
    QVERIFY2(dpr > 0.0, "devicePixelRatio must be positive");

    if (const QByteArray requested = qgetenv("QT_SCALE_FACTOR"); !requested.isEmpty()) {
        const qreal expected = requested.toDouble();
        QVERIFY2(qAbs(dpr - expected) < 0.01,
                 qPrintable(QStringLiteral("expected QT_SCALE_FACTOR=%1, devicePixelRatio was %2")
                                    .arg(expected)
                                    .arg(dpr)));
    }

    const QStringList regions = { QStringLiteral("sidebar"), QStringLiteral("worksheetArea"),
                                   QStringLiteral("outputPanes"), QStringLiteral("statusBar") };
    for (const QString &name : regions) {
        auto *item = root->findChild<QQuickItem *>(name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QVERIFY2(item->width() > 0.0, qPrintable(name + QStringLiteral(": width is zero")));
        QVERIFY2(item->height() > 0.0, qPrintable(name + QStringLiteral(": height is zero")));
    }

    // Not part of the pass/fail check: manual DPI verification support only
    // (task brief -- "grab the window to PNG under the scratchpad ... report
    // sizes/pixel dimensions; do not commit images"). Inert unless a
    // developer sets this env var by hand; ctest never does.
    if (const QByteArray grabDir = qgetenv("RELDEX_UI_DPI_GRAB_DIR"); !grabDir.isEmpty()) {
        // RELDEX_UI_DPI_GRAB_DARK: also manual-only. QT_QUICK_CONTROLS_COLOR_SCHEME
        // does not affect Application.styleHints.colorScheme (it is a Controls-
        // internal styling hint, not the OS scheme Theme.qml reads), so the only
        // reliable way to grab the dark palette by hand is the same path a user's
        // theme picker takes: AppSettings.themeOverride.
        if (const QByteArray forceDark = qgetenv("RELDEX_UI_DPI_GRAB_DARK");
            !forceDark.isEmpty() && forceDark != "0") {
            auto *settings = engine.singletonInstance<AppSettings *>("Reldex.Adapter", "AppSettings");
            QVERIFY(settings != nullptr);
            settings->setThemeOverride(AppSettings::Dark);
            QCoreApplication::processEvents();
            QCoreApplication::processEvents();
        }

        QDir().mkpath(QString::fromLocal8Bit(grabDir));
        const QImage grab = window->grabWindow();
        const QString path = QDir(QString::fromLocal8Bit(grabDir))
                                      .filePath(QStringLiteral("shell-scale-%1.png").arg(dpr));
        qInfo() << "DPI grab" << path << grab.size() << "devicePixelRatio" << dpr;
        QVERIFY2(grab.save(path), qPrintable(path));
    }
}

void TstCoreInfo::connectionManagerDialogRendersAtCurrentScaleFactor()
{
    // Should-fix (M3.2 fix round, 2026-09-26): the connection-manager dialog
    // (ui/app/ConnectionManagerDialog.qml) had no offscreen/DPI coverage --
    // mirrors appShellRendersAtCurrentScaleFactor() above, scoped to the
    // dialog Sidebar.qml always instantiates (not lazily). Registered in
    // CMakeLists.txt as its own ctest process at QT_SCALE_FACTOR=2, on top of
    // running once more, unfiltered, as part of the plain "tst_coreinfo"
    // entry at the default 1x -- covering "1280x800 and 2x" per the task
    // brief without a third/fourth ctest process.
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);
    QCOMPARE(window->width(), 1280);
    QCOMPARE(window->height(), 800);

    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    ConnectionManager *connections = bridge->connections();
    QVERIFY(connections != nullptr);
    // Main.qml's own Component.onCompleted already called open(); just wait
    // for it to finish (the in-memory test store is instant, but this must
    // never race the drain).
    QVERIFY(adapter_test::spinUntil([connections] { return connections->isReady(); }));

    // `Popup`/`QQuickPopup` is a `QObject`, not a `QQuickItem` (its C++ type
    // is a Qt Quick Controls *private* header, so this stays at the public
    // QObject/QML-property level throughout, deliberately never casting to
    // it) -- findChild<QObject*>(), not findChild<QQuickItem*>(), and every
    // read below goes through property()/invokeMethod() rather than a
    // concrete-type call.
    QObject *dialog = root->findChild<QObject *>(QStringLiteral("connectionManagerDialog"));
    QVERIFY(dialog != nullptr);
    QVERIFY(QMetaObject::invokeMethod(dialog, "open"));
    QCoreApplication::processEvents();
    QCoreApplication::processEvents();
    QVERIFY(dialog->property("opened").toBool());
    // As appShellThaiSampleTextIsNotZeroWidth() above notes: layout (here,
    // the dialog's open transition plus its Column/Flickable content) defers
    // to an actual scene-graph frame tick, which a bare processEvents() can
    // return before -- this needs a real wait.
    QTest::qWait(50);

    QVERIFY2(dialog->property("width").toReal() > 0.0, "connectionManagerDialog: width is zero");
    QVERIFY2(dialog->property("height").toReal() > 0.0, "connectionManagerDialog: height is zero");

    // The Popup's own visual item tree starts at its contentItem, not at the
    // Popup object itself.
    auto *dialogContent = dialog->property("contentItem").value<QQuickItem *>();
    QVERIFY(dialogContent != nullptr);

    // Every control that is visible for a *new* profile (the dialog's state
    // right after open()) -- deleteConnectionButton is deliberately excluded,
    // it is hidden until an existing row is selected.
    const QStringList controls = { QStringLiteral("nameField"), QStringLiteral("environmentCombo"),
                                    QStringLiteral("roleCombo"), QStringLiteral("passwordField"),
                                    QStringLiteral("savePasswordCheckBox"),
                                    QStringLiteral("testConnectButton"),
                                    QStringLiteral("saveConnectionButton"),
                                    QStringLiteral("newConnectionButton") };
    for (const QString &name : controls) {
        // findVisualChild(), not findChild(): several of these are inside a
        // Popup's own visual (not QObject) child tree, same reason
        // appShellThaiSampleTextIsNotZeroWidth() above uses it for a
        // ListView delegate.
        auto *item = adapter_test::findVisualChild(dialogContent, name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QVERIFY2(item->width() > 0.0, qPrintable(name + QStringLiteral(": width is zero")));
        QVERIFY2(item->height() > 0.0, qPrintable(name + QStringLiteral(": height is zero")));
    }

    const qreal dpr = window->devicePixelRatio();
    QVERIFY2(dpr > 0.0, "devicePixelRatio must be positive");
    if (const QByteArray requested = qgetenv("QT_SCALE_FACTOR"); !requested.isEmpty()) {
        const qreal expected = requested.toDouble();
        QVERIFY2(qAbs(dpr - expected) < 0.01,
                 qPrintable(QStringLiteral("expected QT_SCALE_FACTOR=%1, devicePixelRatio was %2")
                                    .arg(expected)
                                    .arg(dpr)));
    }
}

void TstCoreInfo::objectBrowserTreeKeyboardNavigationExpandsAndActivates()
{
    // M6.1 minimal keyboard wiring (should-fix from the PR #47 review; the
    // rest -- Narrator verification, any polish -- is M6.4's, see
    // `ui/README.md` "Object browser (M6.1)"). Real synthetic key events
    // (not `QMetaObject::invokeMethod()` on the QML function directly, unlike
    // `appShellSidebarAndOutputPaneCollapseAndExpand()`'s Ctrl+B/J check):
    // those are `Shortcut` items, which need the *window* to be the OS-active
    // one and were found not reliable enough across CI platforms for that;
    // `Keys.onPressed`/`TreeView`'s own key handling instead need only the
    // *item* to have Qt Quick's internal active focus, which
    // `qWaitForWindowActive()` plus `forceActiveFocus()` makes deterministic
    // offscreen.
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);
    QVERIFY(QTest::qWaitForWindowActive(window));

    auto *tree = root->findChild<QQuickItem *>(QStringLiteral("objectBrowserTree"));
    QVERIFY(tree != nullptr);
    auto *browserModel = root->findChild<ObjectBrowserModel *>(QStringLiteral("objectBrowserModel"));
    QVERIFY(browserModel != nullptr);
    auto *refreshButton = root->findChild<QQuickItem *>(QStringLiteral("objectBrowserRefreshButton"));
    QVERIFY(refreshButton != nullptr);

    tree->forceActiveFocus();
    QVERIFY(tree->hasActiveFocus());
    // Nothing activated yet: the Refresh button (enabled: root.activeIndex
    // !== null) proves it, without reaching into ObjectBrowserPanel.qml's
    // own `root.activeIndex` from C++.
    QVERIFY(!refreshButton->property("enabled").toBool());

    // Down: TreeView's own built-in key handling (enabled by the
    // `selectionModel` this task added) moves the current row onto row 0
    // (the always-present "Connection" node -- expandable immediately, per
    // `hasChildren()`, before anything has been fetched).
    QTest::keyClick(window, Qt::Key_Down);
    const QModelIndex connection = browserModel->index(0, 0);
    QCOMPARE(browserModel->rowCount(connection), 0); // not expanded yet

    // Right: TreeView's built-in expand-on-Right fires `onExpanded`, which
    // calls the same `browserModel.expand()` a click on the disclosure arrow
    // would -- observable here as the Connection row starting to load (it
    // cannot succeed against any driver this build can open, see
    // `ObjectBrowserModel.h`'s top-of-file doc comment, so "settles into
    // HasErrorRole" is what "the fetch really ran" looks like from outside).
    QTest::keyClick(window, Qt::Key_Right);
    QVERIFY(adapter_test::spinUntil(
            [&] { return browserModel->data(connection, ObjectBrowserModel::HasErrorRole).toBool(); },
            30000));

    // Return: this panel's own activation path (`root.activateModelIndex()`),
    // reached through `Keys.onReturnPressed` on `tree` -- the same call the
    // `TapHandler` on a delegate makes. `root.activeIndex` becoming non-null
    // is what enables the Refresh button and the columns pane, so it is
    // externally observable without reaching into the QML file's own
    // property.
    QTest::keyClick(window, Qt::Key_Return);
    QCoreApplication::processEvents();
    QVERIFY(refreshButton->property("enabled").toBool());
}

namespace {

/// The three places `ui/README.md` "Production indicator (M3.4)" lists.
const QStringList kProductionIndicatorNames = { QStringLiteral("productionIndicatorStatusBar"),
                                                 QStringLiteral("productionIndicatorWorksheetHeader"),
                                                 QStringLiteral("productionIndicatorTabBadge") };

} // namespace

void TstCoreInfo::productionIndicatorHiddenByDefault()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);
    QCoreApplication::processEvents();
    QTest::qWait(50);

    // Nothing has called `SessionController::setActiveProfileIsProduction()`
    // yet (M3.3 is the class that will), so this is the "no active profile /
    // non-production profile" case from the task brief.
    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    QVERIFY(bridge->session() != nullptr);
    QVERIFY(!bridge->session()->activeProfileIsProduction());

    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);
    for (const QString &name : kProductionIndicatorNames) {
        auto *item = adapter_test::findVisualChild(rootItem, name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QVERIFY2(!item->isVisible(),
                 qPrintable(name + QStringLiteral(": should be hidden when not production")));
    }

    // The worksheet-header strip also takes no layout space when hidden
    // (`ui/app/WorksheetArea.qml`'s `worksheetHeader.height` binding).
    auto *header = root->findChild<QQuickItem *>(QStringLiteral("worksheetHeader"));
    QVERIFY(header != nullptr);
    QCOMPARE(header->height(), 0.0);
}

void TstCoreInfo::productionIndicatorVisibleInAllThreePlacesWhenActive()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);

    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    SessionController *session = bridge->session();
    QVERIFY(session != nullptr);

    session->setActiveProfileIsProduction(true);
    QCoreApplication::processEvents();
    QCoreApplication::processEvents();
    QTest::qWait(50);

    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);
    for (const QString &name : kProductionIndicatorNames) {
        auto *item = adapter_test::findVisualChild(rootItem, name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QVERIFY2(item->isVisible(),
                 qPrintable(name + QStringLiteral(": should be visible when production")));

        // Icon *and* text, never colour alone (phase-1.md row M3.4).
        auto *icon = adapter_test::findVisualChild(item, QStringLiteral("productionIndicatorIcon"));
        auto *label = adapter_test::findVisualChild(item, QStringLiteral("productionIndicatorLabel"));
        QVERIFY2(icon != nullptr, qPrintable(name));
        QVERIFY2(label != nullptr, qPrintable(name));
        QVERIFY2(icon->property("contentWidth").toReal() > 0.0,
                 qPrintable(name + QStringLiteral(": icon did not render")));
        QVERIFY2(label->property("contentWidth").toReal() > 0.0,
                 qPrintable(name + QStringLiteral(": label did not render")));

        const QString text = label->property("text").toString();
        QVERIFY2(text == QStringLiteral("PRODUCTION") || text == QStringLiteral("PROD"),
                 qPrintable(name + QStringLiteral(": unexpected label text '") + text
                            + QStringLiteral("'")));
    }

    auto *header = root->findChild<QQuickItem *>(QStringLiteral("worksheetHeader"));
    QVERIFY(header != nullptr);
    QVERIFY2(header->height() > 0.0, "worksheetHeader: height should be nonzero when active");

    session->setActiveProfileIsProduction(false);
}

void TstCoreInfo::productionIndicatorFollowsTheFlagNotTheEnvironment()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);

    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    SessionController *session = bridge->session();
    QVERIFY(session != nullptr);

    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);

    // Stands in for a Custom-environment profile with `treat_as_production`
    // toggled on, then off (ADR-0006 P3): the adapter hands this layer only
    // the resolved bool, never `ReldexEnvironmentKind`, so a named
    // environment (always on/off) and a Custom one (the user's choice) are
    // indistinguishable from here -- which is exactly the point.
    for (const bool production : { true, false, true, false }) {
        session->setActiveProfileIsProduction(production);
        QCoreApplication::processEvents();
        QCoreApplication::processEvents();

        for (const QString &name : kProductionIndicatorNames) {
            auto *item = adapter_test::findVisualChild(rootItem, name);
            QVERIFY2(item != nullptr, qPrintable(name));
            QCOMPARE(item->isVisible(), production);
        }
    }
}

void TstCoreInfo::productionIndicatorPassesGreyscaleCheck()
{
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);

    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    SessionController *session = bridge->session();
    QVERIFY(session != nullptr);
    session->setActiveProfileIsProduction(true);
    QCoreApplication::processEvents();
    QCoreApplication::processEvents();
    QTest::qWait(50);

    auto *statusBarItem = root->findChild<QQuickItem *>(QStringLiteral("statusBar"));
    QVERIFY(statusBarItem != nullptr);
    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);
    auto *indicator =
            adapter_test::findVisualChild(rootItem, QStringLiteral("productionIndicatorStatusBar"));
    QVERIFY(indicator != nullptr);
    QVERIFY(indicator->isVisible());
    QVERIFY2(indicator->width() > 0.0 && indicator->height() > 0.0,
             "productionIndicatorStatusBar: zero-size geometry");

    const QImage grab = window->grabWindow();
    QVERIFY(!grab.isNull());
    // Desaturate exactly as a colour-blind or monochrome-display user would
    // see it -- the task brief's own wording ("convert to greyscale").
    const QImage grey = grab.convertToFormat(QImage::Format_Grayscale8);
    QVERIFY(!grey.isNull());

    const qreal dpr = window->devicePixelRatio();

    // The indicator's own bounding box, scene -> grabbed-image pixels.
    const QPointF indicatorScenePos = indicator->mapToScene(QPointF(0, 0));
    const int ix0 = qMax(0, static_cast<int>(indicatorScenePos.x() * dpr));
    const int iy0 = qMax(0, static_cast<int>(indicatorScenePos.y() * dpr));
    const int ix1 = qMin(grey.width(), ix0 + qMax(1, static_cast<int>(indicator->width() * dpr)));
    const int iy1 = qMin(grey.height(), iy0 + qMax(1, static_cast<int>(indicator->height() * dpr)));
    QVERIFY2(ix1 > ix0 && iy1 > iy0, "productionIndicatorStatusBar: geometry maps outside the grab");

    // The darkest pixel anywhere inside that box is the icon/label ink --
    // whichever of the two rendered a fully-covered pixel closest to
    // `Theme.tokens.warning`.
    int darkestIndicatorGrey = 255;
    for (int y = iy0; y < iy1; ++y) {
        for (int x = ix0; x < ix1; ++x) {
            darkestIndicatorGrey = qMin(darkestIndicatorGrey, qGray(grey.pixel(x, y)));
        }
    }

    // A background sample from the status bar's own plain fill, at a point
    // no child item reaches: (2, 2) is inside the 1px top divider's shadow
    // but past it, and well left of the Row's own 8px left margin.
    const QPointF backgroundScenePos = statusBarItem->mapToScene(QPointF(2, 2));
    const int bx = qBound(0, static_cast<int>(backgroundScenePos.x() * dpr), grey.width() - 1);
    const int by = qBound(0, static_cast<int>(backgroundScenePos.y() * dpr), grey.height() - 1);
    const int backgroundGrey = qGray(grey.pixel(bx, by));

    qInfo() << "productionIndicatorPassesGreyscaleCheck: darkest indicator pixel"
            << darkestIndicatorGrey << "background" << backgroundGrey;

    // A presence/contrast assertion, not a pixel-diff (task brief): the
    // indicator's ink must read as meaningfully darker than the plain
    // background once colour is gone, by a margin well past antialiasing
    // noise. `Theme.tokens.warning` against `surfaceAlt` clears this by a
    // wide margin in both palettes (see ui/README.md "Production indicator
    // (M3.4)").
    QVERIFY2(backgroundGrey - darkestIndicatorGrey >= 24,
             qPrintable(QStringLiteral("indicator not distinguishable from its background in "
                                       "greyscale: darkest-indicator=%1 background=%2")
                                .arg(darkestIndicatorGrey)
                                .arg(backgroundGrey)));

    session->setActiveProfileIsProduction(false);
}

void TstCoreInfo::productionIndicatorRendersAtCurrentScaleFactor()
{
    // Mirrors appShellRendersAtCurrentScaleFactor()/
    // connectionManagerDialogRendersAtCurrentScaleFactor() above: registered
    // in ui/tests/CMakeLists.txt at the default 1x (as part of the plain
    // "tst_coreinfo" entry) and once more at QT_SCALE_FACTOR=2.
    QQmlApplicationEngine engine;
    engine.loadFromModule("Reldex.App", "Main");
    QObject *root = engine.rootObjects().constFirst();
    QVERIFY(root != nullptr);
    auto *window = qobject_cast<QQuickWindow *>(root);
    QVERIFY(window != nullptr);
    QCoreApplication::processEvents();
    QCoreApplication::processEvents();

    auto *bridge = root->findChild<Bridge *>(QStringLiteral("bridge"));
    QVERIFY(bridge != nullptr);
    SessionController *session = bridge->session();
    QVERIFY(session != nullptr);
    session->setActiveProfileIsProduction(true);
    QCoreApplication::processEvents();
    QTest::qWait(50);

    const qreal dpr = window->devicePixelRatio();
    QVERIFY2(dpr > 0.0, "devicePixelRatio must be positive");
    if (const QByteArray requested = qgetenv("QT_SCALE_FACTOR"); !requested.isEmpty()) {
        const qreal expected = requested.toDouble();
        QVERIFY2(qAbs(dpr - expected) < 0.01,
                 qPrintable(QStringLiteral("expected QT_SCALE_FACTOR=%1, devicePixelRatio was %2")
                                    .arg(expected)
                                    .arg(dpr)));
    }

    QQuickItem *rootItem = window->contentItem();
    QVERIFY(rootItem != nullptr);
    for (const QString &name : kProductionIndicatorNames) {
        auto *item = adapter_test::findVisualChild(rootItem, name);
        QVERIFY2(item != nullptr, qPrintable(name));
        QVERIFY2(item->isVisible(), qPrintable(name));
        QVERIFY2(item->width() > 0.0, qPrintable(name + QStringLiteral(": width is zero")));
        QVERIFY2(item->height() > 0.0, qPrintable(name + QStringLiteral(": height is zero")));
    }

    session->setActiveProfileIsProduction(false);
}

QTEST_MAIN(TstCoreInfo)

#include "tst_coreinfo.moc"
