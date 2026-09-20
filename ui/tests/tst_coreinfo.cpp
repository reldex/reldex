#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QQmlError>
#include <QSignalSpy>
#include <QTest>

#include <reldex.h>

// M1.5 acceptance criteria (docs/exec-plans/active/phase-1.md, row M1.5):
//   (a) reldex_abi_version() equals the header's ABI version constant.
//   (b) the QML module / Main.qml loads via QQmlApplicationEngine under
//       QT_QPA_PLATFORM=offscreen, with the root object created and no QML
//       warnings.
// No hub, no session -- that scope belongs to M1.6's model tests.
class TstCoreInfo : public QObject
{
    Q_OBJECT

private slots:
    void abiVersionMatchesHeader();
    void qmlModuleLoadsCleanly();
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

QTEST_MAIN(TstCoreInfo)

#include "tst_coreinfo.moc"
