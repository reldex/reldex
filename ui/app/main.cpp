#include <QByteArray>
#include <QFile>
#include <QGuiApplication>
#include <QMutex>
#include <QQmlApplicationEngine>
#include <QSurfaceFormat>
#include <QTextStream>

namespace {

// Spike S15 (M1.8), off by default: a file sink for Qt's own logging
// categories.
//
// It exists because `Reldex.exe` is a GUI-subsystem binary, so `qDebug` output
// has no console to reach and no pipe to be captured from -- which makes
// `QT_LOGGING_RULES="qt.scenegraph.time.*=true"`, the only way to see what the
// cold first frame is made of, produce nothing at all from a terminal. With
// `RELDEX_S15_LOG` unset this handler is never installed and the default one
// stays in place.
//
// It records; it does not filter. Which categories are on is `QT_LOGGING_RULES`'
// business, so the file contains exactly what Qt emitted.
QFile *g_logFile = nullptr;
QMutex g_logMutex;
QtMessageHandler g_previousHandler = nullptr;

void fileMessageHandler(QtMsgType type, const QMessageLogContext &context, const QString &message)
{
    {
        const QMutexLocker locker(&g_logMutex);
        if (g_logFile != nullptr && g_logFile->isOpen()) {
            QTextStream stream(g_logFile);
            stream << qFormatLogMessage(type, context, message) << '\n';
            stream.flush();
        }
    }
    if (g_previousHandler != nullptr) {
        g_previousHandler(type, context, message);
    }
}

} // namespace

int main(int argc, char *argv[])
{
    if (const QByteArray logPath = qgetenv("RELDEX_S15_LOG"); !logPath.isEmpty()) {
        g_logFile = new QFile(QString::fromLocal8Bit(logPath));
        if (g_logFile->open(QIODevice::WriteOnly | QIODevice::Truncate | QIODevice::Text)) {
            g_previousHandler = qInstallMessageHandler(fileMessageHandler);
        } else {
            delete g_logFile;
            g_logFile = nullptr;
        }
    }

    // Spike S15 (M1.8), off by default. With vsync on, a frame-swap interval
    // quantizes to the refresh period, so it answers "did the frame make its
    // budget?" and cannot answer "how much work was the frame?" -- and K1 is
    // written with clauses about both. Setting the default swap interval to 0
    // makes the scene graph present without waiting (Qt maps swapInterval 0 to
    // QRhiSwapChain::NoVSync), so a pass with this on measures the frame rate
    // the machine can actually produce. It changes nothing about how the app
    // renders, only when it presents, and it is a process-local request: no
    // display, driver or system setting is touched.
    if (const QByteArray noVsync = qgetenv("RELDEX_S15_NO_VSYNC");
        !noVsync.isEmpty() && noVsync != "0") {
        QSurfaceFormat format = QSurfaceFormat::defaultFormat();
        format.setSwapInterval(0);
        QSurfaceFormat::setDefaultFormat(format);
    }

    QGuiApplication app(argc, argv);

    QQmlApplicationEngine engine;
    QObject::connect(
        &engine, &QQmlApplicationEngine::objectCreationFailed, &app,
        []() { QCoreApplication::exit(-1); }, Qt::QueuedConnection);

    engine.loadFromModule("Reldex.App", "Main");

    const int status = app.exec();

    if (g_logFile != nullptr) {
        qInstallMessageHandler(g_previousHandler);
        const QMutexLocker locker(&g_logMutex);
        g_logFile->close();
        delete g_logFile;
        g_logFile = nullptr;
    }
    return status;
}
