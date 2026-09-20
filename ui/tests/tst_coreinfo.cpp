#include "AdapterTestSupport.h"

#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QQmlError>
#include <QSignalSpy>
#include <QTest>
#include <QtQuick/QQuickItem>

#include <reldex.h>

// M1.5 acceptance criteria (docs/exec-plans/active/phase-1.md, row M1.5):
//   (a) reldex_abi_version() equals the header's ABI version constant.
//   (b) the QML module / Main.qml loads via QQmlApplicationEngine under
//       QT_QPA_PLATFORM=offscreen, with the root object created and no QML
//       warnings.
// M1.6 adds (c): the QML surface really is bound to the real model -- a query
// runs through the loaded tree and its column headers come back through QML.
class TstCoreInfo : public QObject
{
    Q_OBJECT

private slots:
    void abiVersionMatchesHeader();
    void qmlModuleLoadsCleanly();
    void theQmlSurfaceIsBoundToTheRealModel();
};

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

void TstCoreInfo::theQmlSurfaceIsBoundToTheRealModel()
{
    QQmlApplicationEngine engine;

    QList<QQmlError> warnings;
    connect(&engine, &QQmlEngine::warnings, &engine,
            [&warnings](const QList<QQmlError> &reported) { warnings += reported; });

    engine.loadFromModule("Reldex.App", "Main");
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

QTEST_MAIN(TstCoreInfo)

#include "tst_coreinfo.moc"
