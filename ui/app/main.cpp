#include <QByteArray>
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QSurfaceFormat>

int main(int argc, char *argv[])
{
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

    return app.exec();
}
