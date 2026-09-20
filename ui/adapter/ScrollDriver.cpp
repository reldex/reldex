#include "ScrollDriver.h"

#include "Bridge.h"
#include "Metrics.h"
#include "SessionController.h"

#include <QByteArray>
#include <QFile>
#include <QGuiApplication>
#include <QJsonDocument>
#include <QScreen>
#include <QSurfaceFormat>
#include <QTimer>
#include <QtQuick/QSGRendererInterface>

#include <algorithm>
#include <cmath>

#ifdef Q_OS_WIN
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#endif

namespace {

qint64 envNumber(const char *name, qint64 fallback)
{
    const QByteArray value = qgetenv(name);
    if (value.isEmpty()) {
        return fallback;
    }
    bool ok = false;
    const qint64 parsed = value.toLongLong(&ok);
    return ok ? parsed : fallback;
}

double envDouble(const char *name, double fallback)
{
    const QByteArray value = qgetenv(name);
    if (value.isEmpty()) {
        return fallback;
    }
    bool ok = false;
    const double parsed = value.toDouble(&ok);
    return ok ? parsed : fallback;
}

QString envString(const char *name)
{
    return QString::fromLocal8Bit(qgetenv(name));
}

const char *graphicsApiName(QSGRendererInterface::GraphicsApi api)
{
    switch (api) {
    case QSGRendererInterface::Software:
        return "software";
    case QSGRendererInterface::OpenGL:
        return "opengl";
    case QSGRendererInterface::Direct3D11:
        return "d3d11";
    case QSGRendererInterface::Direct3D12:
        return "d3d12";
    case QSGRendererInterface::Vulkan:
        return "vulkan";
    case QSGRendererInterface::Metal:
        return "metal";
    case QSGRendererInterface::Null:
        return "null";
    default:
        break;
    }
    return "unknown";
}

} // namespace

ScrollDriver::ScrollDriver(Bridge *bridge, QObject *parent)
    : QObject(parent)
    , m_bridge(bridge)
{
    const QString patterns = envString("RELDEX_S15_SCROLL");
    m_runs = static_cast<int>(std::clamp<qint64>(envNumber("RELDEX_S15_RUNS", 0), 0, 10000));
    // Nothing asked for: stay completely inert. This is the normal case, and
    // it is the whole reason the driver may live in the shipped adapter.
    if (patterns.isEmpty() && m_runs == 0) {
        return;
    }
    m_enabled = true;
    m_runs = std::max(m_runs, 1);

    const QStringList names = patterns.split(QLatin1Char(','), Qt::SkipEmptyParts);
    for (const QString &raw : names) {
        const QString name = raw.trimmed().toLower();
        Pattern pattern = Pattern::Idle;
        if (name == QLatin1String("flick")) {
            pattern = Pattern::Flick;
        } else if (name == QLatin1String("sweep")) {
            pattern = Pattern::Sweep;
        } else if (name == QLatin1String("full")) {
            pattern = Pattern::Full;
        } else if (name == QLatin1String("jump")) {
            pattern = Pattern::Jump;
        } else if (name == QLatin1String("blocksame")) {
            pattern = Pattern::BlockSame;
        } else if (name == QLatin1String("blockother")) {
            pattern = Pattern::BlockOther;
        } else if (name == QLatin1String("idle")) {
            pattern = Pattern::Idle;
        } else {
            qWarning("RELDEX_S15_SCROLL: unknown pattern \"%s\"; treating it as idle",
                     qPrintable(name));
        }
        m_phases.append(Phase { name, pattern });
    }

    m_outPath = envString("RELDEX_S15_OUT");
    m_csvPath = envString("RELDEX_S15_CSV");
    m_label = envString("RELDEX_S15_LABEL");
    m_phaseFrames = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_SCROLL_FRAMES", 1800), 1, 1000000));
    m_warmupFrames = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_WARMUP_FRAMES", 30), 0, 100000));
    m_stepPx = envDouble("RELDEX_S15_SCROLL_STEP_PX", 110.0);
    m_flickVelocity = envDouble("RELDEX_S15_FLICK_VELOCITY", 9000.0);
    m_settleMs = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_SETTLE_MS", 2000), 0, 600000));
    // 0 is the mock's "block until released", which is what a deterministic
    // measurement wants: the driver releases it at the end of the phase, so
    // the blocked window is exactly the measured window rather than a
    // stopwatch race. A non-zero value blocks for that long instead.
    m_blockMs = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_BLOCK_MS", 0), 0, 600000));
    m_quitWhenDone = envNumber("RELDEX_S15_NO_QUIT", 0) == 0;
    m_topmost = envNumber("RELDEX_S15_NO_TOPMOST", 0) == 0;
    m_keepDisplayAwake = envNumber("RELDEX_S15_NO_KEEPAWAKE", 0) == 0;
    m_startDelayMs = static_cast<int>(
            std::clamp<qint64>(envNumber("RELDEX_S15_START_DELAY_MS", 1500), 0, 600000));
    m_jumpState ^= static_cast<quint64>(envNumber("RELDEX_S15_SEED", 0)) * 0x9E3779B97F4A7C15ULL;
    if (m_jumpState == 0) {
        m_jumpState = 0x243F6A8885A308D3ULL;
    }
}

ScrollDriver::~ScrollDriver()
{
#ifdef Q_OS_WIN
    if (m_displayRequested) {
        // Give the power state back exactly as it was found.
        ::SetThreadExecutionState(ES_CONTINUOUS);
    }
#endif
}

void ScrollDriver::requestDisplayStaysOn()
{
#ifdef Q_OS_WIN
    if (!m_keepDisplayAwake || m_displayRequested) {
        return;
    }
    // A frame-time measurement needs the display to be on: with the monitor
    // asleep the graphics stack throttles presentation (measured on this
    // machine: 252 ms per frame, i.e. ~4 Hz, for a frame whose own work was
    // 0.3 ms), and a p99 taken through that would be a measurement of Windows'
    // power management.
    //
    // This is the documented, **process-local** way to say "I am showing
    // something on screen" -- the same call a video player makes. It changes
    // no user or machine setting, it is undone in the destructor, and per its
    // own documentation it prevents the display from turning off; it does not
    // turn an already-off display back on. So it keeps a measurement run
    // valid; it cannot rescue one that started with the screen asleep, and the
    // report says which of the two happened.
    m_displayRequested = ::SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED
                                                   | ES_SYSTEM_REQUIRED)
            != 0;
#endif
}

void ScrollDriver::attach(QQuickWindow *window, QQuickItem *flickable)
{
    if (!m_enabled || m_attached || m_bridge == nullptr || !m_bridge->isValid()) {
        return;
    }
    m_attached = true;
    m_window = window;
    m_flickable = flickable;

    requestDisplayStaysOn();
    m_baselineResidentBytes = Metrics::residentBytes();
    m_baselinePrivateBytes = Metrics::privateBytes();
    // K3 asks for the memory **the rows** cost, and a baseline taken here --
    // inside `Component.onCompleted`, before the window has rendered a single
    // frame -- charges the scene graph's, the RHI's and the graphics driver's
    // one-time allocations to the result set. Measured, that is not small: the
    // same 1,000,000-row stream reads as +200.7 MB from here and +113 MB from
    // an idle app that has already drawn. So the driver takes a second
    // baseline once the app is idle and drawn, and owns the start of the run
    // itself, which is the only way to have a "before" that is genuinely
    // before the rows and genuinely after the window.
    if (m_startDelayMs > 0) {
        m_ownsStart = true;
        QTimer::singleShot(m_startDelayMs, this, &ScrollDriver::startFirstRun);
    }

    // Recording has to be on for any of this to mean anything; a run that set
    // RELDEX_S15_SCROLL and forgot RELDEX_UI_METRICS would otherwise produce a
    // file full of zeroes that looks like a result.
    m_bridge->metrics()->setEnabled(true);

    connect(m_bridge->metrics(), &Metrics::firstFrameAfterInsert, this,
            &ScrollDriver::onFirstFrameAfterInsert);
    connect(m_bridge->session(), &SessionController::resultComplete, this,
            &ScrollDriver::onResultComplete);
    if (m_window != nullptr) {
        connect(m_window, &QQuickWindow::afterAnimating, this, &ScrollDriver::onAfterAnimating);
        raiseWindow();
    }
}

void ScrollDriver::raiseWindow()
{
    if (m_window == nullptr) {
        return;
    }
    // Not cosmetic, and not optional: a window the compositor considers
    // **occluded** is throttled by the graphics stack rather than by this
    // application. Measured on this machine, behind another window: every
    // frame -- including a frame with no motion in it at all -- took 252 ms,
    // i.e. presentation pinned at ~4 Hz, while the scene graph's own work was
    // 0.3 ms. A frame-time criterion measured through that throttle would be a
    // measurement of the window manager.
    //
    // So a measurement run puts its own window in front, at the start of every
    // phase, and the report says it did. Nothing system-wide is changed: this
    // is the same request any application makes when it opens a window.
    //
    // `raise()` + `requestActivate()` alone are not enough here and that is not
    // a Qt problem: Windows refuses a foreground change requested by a process
    // that does not own the foreground, so a window launched from a terminal
    // stays behind the terminal (measured: visible, not minimized, not
    // foreground -- and throttled). The always-on-top hint does not need
    // foreground rights, so it is what actually uncovers the window. It is set
    // once, it is confined to this process's own window, and
    // `RELDEX_S15_NO_TOPMOST=1` turns it off for a run that wants to see the
    // throttled behaviour instead.
    if (m_topmost && !m_topmostSet) {
        m_topmostSet = true;
        m_window->setFlag(Qt::WindowStaysOnTopHint, true);
    }
    m_window->raise();
    m_window->requestActivate();
}

void ScrollDriver::onFirstFrameAfterInsert()
{
    if (!m_enabled || m_finished) {
        return;
    }
    if (m_runsDone >= m_runs) {
        // K2 accounting is over. The signal keeps arriving while the stream
        // runs -- every batch re-arms "rows have been inserted" -- and
        // recording those as runs would fill `runs` with entries that have no
        // execute of their own. Found by reading 85 runs out of a
        // single-run configuration, not by reasoning about it first.
        return;
    }
    m_bridge->metrics()->endRun();
    ++m_runsDone;
    if (m_runsDone < m_runs) {
        // Submitted **synchronously**, not through a zero-millisecond timer:
        // the gap would let one more batch of the result being replaced land
        // between "clear the marks" and "record the execute", which sets
        // `firstRowsInserted` before `executeSubmitted` and makes the run's
        // latency negative. Nothing else runs on this thread between these two
        // calls, so the window does not exist.
        startNextRun();
        return;
    }
    if (m_phases.isEmpty()) {
        finish();
    }
    // Otherwise the last run is left to stream to completion; `resultComplete`
    // starts the scroll phases.
}

void ScrollDriver::startNextRun()
{
    if (m_finished) {
        return;
    }
    m_bridge->run();
}

void ScrollDriver::startFirstRun()
{
    if (m_finished) {
        return;
    }
    m_preRunResidentBytes = Metrics::residentBytes();
    m_preRunPrivateBytes = Metrics::privateBytes();
    m_bridge->run();
}

void ScrollDriver::onResultComplete()
{
    if (!m_enabled || m_finished || m_phases.isEmpty() || m_phaseIndex >= 0) {
        return;
    }
    // Let the stream's allocations settle before anything is read as "retained
    // memory for 1M rows", and before the first scroll frame is timed.
    QTimer::singleShot(m_settleMs, this, &ScrollDriver::beginPhases);
}

void ScrollDriver::beginPhases()
{
    if (m_finished || m_phaseIndex >= 0) {
        return;
    }
    m_memory.insert(QStringLiteral("baselineResidentBytes"), m_baselineResidentBytes);
    m_memory.insert(QStringLiteral("baselinePrivateBytes"), m_baselinePrivateBytes);
    m_memory.insert(QStringLiteral("preRunResidentBytes"), m_preRunResidentBytes);
    m_memory.insert(QStringLiteral("preRunPrivateBytes"), m_preRunPrivateBytes);
    m_memory.insert(QStringLiteral("startDelayMs"), m_startDelayMs);
    m_memory.insert(QStringLiteral("settledResidentBytes"), Metrics::residentBytes());
    m_memory.insert(QStringLiteral("settledPrivateBytes"), Metrics::privateBytes());
    m_memory.insert(QStringLiteral("settleMs"), m_settleMs);

    const bool needsOther = std::any_of(m_phases.cbegin(), m_phases.cend(), [](const Phase &phase) {
        return phase.pattern == Pattern::BlockOther;
    });
    if (needsOther) {
        // Opened now, so the phase that blocks it is not also timing a session
        // open. A second Bridge is a second hub, a second waker and a second
        // pump thread -- which is exactly what K6 asks about.
        m_otherBridge = new Bridge(this);
        if (m_otherBridge->isValid()) {
            m_otherBridge->session()->setMockBlockDurationMs(m_blockMs);
            m_otherBridge->session()->setRunOnOpen(false);
            m_otherBridge->session()->open();
        }
    }

    m_phaseIndex = 0;
    beginPhase();
}

void ScrollDriver::beginPhase()
{
    if (m_phaseIndex >= m_phases.size()) {
        finish();
        return;
    }
    m_frameIndex = 0;
    m_direction = 1;
    m_travelledPx = 0.0;
    m_blockSubmitted = false;
    m_flickVelocityNow = m_flickVelocity;
    m_phaseMaxY = maxContentY();
    if (m_window != nullptr && m_window->screen() != nullptr
        && m_window->screen()->refreshRate() > 1.0) {
        m_refreshHz = m_window->screen()->refreshRate();
    }
    const Phase &phase = m_phases.at(m_phaseIndex);
    m_phaseStepPx = phase.pattern == Pattern::Full
            ? m_phaseMaxY / std::max(1.0, static_cast<double>(m_phaseFrames) / 2.0)
            : m_stepPx;
    setContentY(0.0);
    m_phaseStartNs = m_bridge->metrics()->nowNs();
    raiseWindow();
    if (m_window != nullptr) {
        m_window->requestUpdate();
    }
}

void ScrollDriver::onAfterAnimating()
{
    if (m_finished || m_phaseIndex < 0 || m_phaseIndex >= m_phases.size()) {
        return;
    }
    const Phase &phase = m_phases.at(m_phaseIndex);

    ++m_frameIndex;
    if (m_frameIndex == m_warmupFrames) {
        // The phase's own start-up -- the first table rebuild at a new
        // position, the first formatted windows -- is not what the phase is
        // measuring, so those frames are dropped here rather than being
        // averaged away later.
        m_bridge->metrics()->clearFrames();
        m_phaseStartNs = m_bridge->metrics()->nowNs();
        m_travelledPx = 0.0;
        submitBlock(phase.pattern);
    }

    advance();

    if (m_frameIndex >= m_warmupFrames + m_phaseFrames) {
        endPhase();
        return;
    }
    if (m_window != nullptr) {
        // Keep frames coming even when the pattern did not dirty anything
        // (idle, or a flick that has come to rest): a phase must time a fixed
        // number of frames, not a fixed number of repaints.
        m_window->requestUpdate();
    }
}

void ScrollDriver::advance()
{
    if (m_flickable == nullptr || m_phaseIndex < 0) {
        return;
    }
    const Pattern pattern = m_phases.at(m_phaseIndex).pattern;
    switch (pattern) {
    case Pattern::Idle:
        return;

    case Pattern::Flick: {
        // A flick-shaped velocity profile applied to `contentY`, not a call to
        // `Flickable::flick()`.
        //
        // The call was tried first and is recorded here because it failed
        // silently: invoked from `afterAnimating` -- which is inside the
        // polish-and-sync cycle -- the view did not move at all (final
        // contentY 0, scene-graph work 0.037 ms/frame, i.e. a static scene
        // dressed up as a scroll). A pattern that flatters the result by not
        // moving is the worst possible outcome for this measurement, so the
        // motion is generated here where it is visible and checkable, and the
        // phase reports the contentY it actually reached.
        //
        // The profile is Flickable's own: an initial velocity in px/s decaying
        // at `flickDeceleration` (Qt's default is 1500 px/s^2), re-kicked when
        // it dies, reversing at the ends. What it does NOT exercise is
        // Flickable's internal animation; what it does exercise -- a viewport
        // moving a realistic number of rows per frame, the table rebuilding,
        // the model being asked for cells -- is what K1 is about.
        const double seconds = 1.0 / std::max(1.0, m_refreshHz);
        if (m_flickVelocityNow <= 0.0) {
            m_flickVelocityNow = m_flickVelocity;
        }
        double y = contentY() + (m_flickVelocityNow * seconds * m_direction);
        m_flickVelocityNow -= kFlickDeceleration * seconds;
        if (m_flickVelocityNow < kFlickMinVelocity) {
            m_flickVelocityNow = m_flickVelocity; // the next flick of the wrist
        }
        if (y >= m_phaseMaxY) {
            y = m_phaseMaxY;
            m_direction = -1;
            m_flickVelocityNow = m_flickVelocity;
        } else if (y <= 0.0) {
            y = 0.0;
            m_direction = 1;
            m_flickVelocityNow = m_flickVelocity;
        }
        m_travelledPx += std::abs(y - contentY());
        setContentY(y);
        return;
    }

    case Pattern::Sweep:
    case Pattern::Full:
    case Pattern::BlockSame:
    case Pattern::BlockOther: {
        double y = contentY() + (m_phaseStepPx * m_direction);
        if (y >= m_phaseMaxY) {
            y = m_phaseMaxY;
            m_direction = -1;
        } else if (y <= 0.0) {
            y = 0.0;
            m_direction = 1;
        }
        m_travelledPx += m_phaseStepPx;
        setContentY(y);
        return;
    }

    case Pattern::Jump: {
        // xorshift64*: deterministic from the seed, so a jump phase is
        // repeatable rather than "random each run".
        m_jumpState ^= m_jumpState >> 12;
        m_jumpState ^= m_jumpState << 25;
        m_jumpState ^= m_jumpState >> 27;
        const double unit = static_cast<double>((m_jumpState * 0x2545F4914F6CDD1DULL) >> 11)
                / static_cast<double>(1ULL << 53);
        const double y = unit * m_phaseMaxY;
        m_travelledPx += std::abs(y - contentY());
        setContentY(y);
        return;
    }
    }
}

void ScrollDriver::submitBlock(Pattern pattern)
{
    if (m_blockSubmitted) {
        return;
    }
    if (pattern == Pattern::BlockSame) {
        m_blockSubmitted = true;
        // Honest note for the report: this adapter holds one result per
        // session, so executing anything on this session closes the 1M-row
        // result. The grid therefore empties -- which is itself part of what
        // K6's same-session case can and cannot answer.
        // The duration is fixed at open time (it is part of the mock scenario
        // in `ReldexOpenOptions`), so this session blocks until released --
        // which `releaseBlock()` does at the end of the phase.
        m_bridge->session()->executeMockStatement(RELDEX_MOCK_STATEMENT_BLOCK);
        return;
    }
    if (pattern == Pattern::BlockOther && m_otherBridge != nullptr
        && m_otherBridge->isValid()) {
        m_blockSubmitted = true;
        m_otherBridge->session()->executeMockStatement(RELDEX_MOCK_STATEMENT_BLOCK);
    }
}

void ScrollDriver::releaseBlock()
{
    if (!m_blockSubmitted) {
        return;
    }
    const Pattern pattern = m_phases.at(m_phaseIndex).pattern;
    if (pattern == Pattern::BlockSame) {
        m_bridge->releaseMockBlock(m_bridge->session()->sessionId());
    } else if (pattern == Pattern::BlockOther && m_otherBridge != nullptr) {
        m_otherBridge->releaseMockBlock(m_otherBridge->session()->sessionId());
    }
    m_blockSubmitted = false;
}

void ScrollDriver::endPhase()
{
    const Phase &phase = m_phases.at(m_phaseIndex);
    Metrics *const metrics = m_bridge->metrics();

    QVariantMap result;
    result.insert(QStringLiteral("phase"), phase.name);
    result.insert(QStringLiteral("frames"), m_phaseFrames);
    result.insert(QStringLiteral("warmupFrames"), m_warmupFrames);
    result.insert(QStringLiteral("wallNs"), metrics->nowNs() - m_phaseStartNs);
    result.insert(QStringLiteral("contentMaxY"), m_phaseMaxY);
    // Where the viewport actually ended up. Recorded because a pattern that
    // silently fails to move produces excellent-looking frame times, and this
    // is the number that gives it away.
    result.insert(QStringLiteral("endContentY"), contentY());
    result.insert(QStringLiteral("stepPx"), m_phaseStepPx);
    result.insert(QStringLiteral("travelledPx"), m_travelledPx);
    // Rows, not pixels, is what "scrolled the full 1M rows" is asked in, and
    // the row height is derived rather than assumed: the delegate's height is
    // Main.qml's business, not this driver's.
    const int modelRows = m_bridge->session()->model()->rowCount();
    const double contentHeight =
            m_flickable == nullptr ? 0.0 : m_flickable->property("contentHeight").toDouble();
    const double rowPx = modelRows > 0 && contentHeight > 0.0
            ? contentHeight / static_cast<double>(modelRows)
            : 0.0;
    result.insert(QStringLiteral("rowPx"), rowPx);
    result.insert(QStringLiteral("rowsTravelled"), rowPx > 0.0 ? m_travelledPx / rowPx : 0.0);
    result.insert(QStringLiteral("frameIntervals"), metrics->frameStats());
    result.insert(QStringLiteral("renderWork"), metrics->renderWorkStats());
    result.insert(QStringLiteral("residentBytes"), Metrics::residentBytes());
    result.insert(QStringLiteral("privateBytes"), Metrics::privateBytes());
    result.insert(QStringLiteral("modelRows"), modelRows);
    result.insert(QStringLiteral("formattedWindows"),
                  m_bridge->session()->model()->formattedWindowCount());
    result.insert(QStringLiteral("formattedBytes"),
                  m_bridge->session()->model()->formattedBytes());
    m_phaseResults.append(result);

    releaseBlock();
    metrics->clearFrames();

    ++m_phaseIndex;
    if (m_phaseIndex >= m_phases.size()) {
        finish();
        return;
    }
    beginPhase();
}

double ScrollDriver::maxContentY() const
{
    if (m_flickable == nullptr) {
        return 0.0;
    }
    const double contentHeight = m_flickable->property("contentHeight").toDouble();
    const double viewHeight = m_flickable->height();
    return std::max(0.0, contentHeight - viewHeight);
}

double ScrollDriver::contentY() const
{
    return m_flickable == nullptr ? 0.0 : m_flickable->property("contentY").toDouble();
}

void ScrollDriver::setContentY(double y)
{
    if (m_flickable != nullptr) {
        m_flickable->setProperty("contentY", y);
    }
}

QVariantMap ScrollDriver::environmentReport() const
{
    QVariantMap map;
    map.insert(QStringLiteral("qtVersion"), QString::fromLatin1(qVersion()));
    map.insert(QStringLiteral("label"), m_label);
    map.insert(QStringLiteral("qtScaleFactor"), envString("QT_SCALE_FACTOR"));
    map.insert(QStringLiteral("qsgRenderLoopEnv"), envString("QSG_RENDER_LOOP"));
    map.insert(QStringLiteral("defaultSwapInterval"),
               QSurfaceFormat::defaultFormat().swapInterval());
    map.insert(QStringLiteral("framesOnGuiThread"), m_bridge->metrics()->framesOnGuiThread());
    if (m_window != nullptr) {
        map.insert(QStringLiteral("devicePixelRatio"), m_window->devicePixelRatio());
        map.insert(QStringLiteral("windowWidth"), m_window->width());
        map.insert(QStringLiteral("windowHeight"), m_window->height());
        if (QSGRendererInterface *renderer = m_window->rendererInterface()) {
            map.insert(QStringLiteral("graphicsApi"),
                       QString::fromLatin1(graphicsApiName(renderer->graphicsApi())));
        }
        if (QScreen *screen = m_window->screen()) {
            map.insert(QStringLiteral("screenName"), screen->name());
            map.insert(QStringLiteral("screenRefreshHz"), screen->refreshRate());
            map.insert(QStringLiteral("screenWidth"), screen->geometry().width());
            map.insert(QStringLiteral("screenHeight"), screen->geometry().height());
            map.insert(QStringLiteral("screenDevicePixelRatio"), screen->devicePixelRatio());
            map.insert(QStringLiteral("screenLogicalDpi"), screen->logicalDotsPerInch());
        }
    }
    SessionController *const session = m_bridge->session();
    map.insert(QStringLiteral("mockRows"), session->mockRows());
    map.insert(QStringLiteral("fetchRows"), session->fetchRows());
    map.insert(QStringLiteral("maxFetchesInFlight"), session->maxFetchesInFlight());
    map.insert(QStringLiteral("perFetchLatencyUs"), session->mockPerFetchLatencyUs());
    map.insert(QStringLiteral("firstBatchLatencyUs"), session->mockFirstBatchLatencyUs());
    return map;
}

void ScrollDriver::finish()
{
    if (m_finished) {
        return;
    }
    m_finished = true;

    Metrics *const metrics = m_bridge->metrics();
    QVariantMap report;
    report.insert(QStringLiteral("environment"), environmentReport());
    report.insert(QStringLiteral("runs"), metrics->runs());
    report.insert(QStringLiteral("phases"), m_phaseResults);
    report.insert(QStringLiteral("memory"), m_memory);
    report.insert(QStringLiteral("summary"), metrics->summary());
    report.insert(QStringLiteral("applyBatch"), metrics->applyBatchStats());
    report.insert(QStringLiteral("drains"), metrics->drainStats());
    report.insert(QStringLiteral("rowsStreamed"), m_bridge->session()->rowsFetched());
    report.insert(QStringLiteral("orphanEvents"), m_bridge->orphanEvents());

    if (!m_csvPath.isEmpty()) {
        metrics->writeCsv(m_csvPath);
    }
    if (!m_outPath.isEmpty()) {
        QFile file(m_outPath);
        if (file.open(QIODevice::WriteOnly | QIODevice::Truncate)) {
            file.write(QJsonDocument(QJsonObject::fromVariantMap(report)).toJson());
            file.close();
        } else {
            qWarning("RELDEX_S15_OUT: could not write \"%s\"", qPrintable(m_outPath));
        }
    }

    if (m_quitWhenDone) {
        // The app opens a real window on someone's desktop; a measurement run
        // closes it rather than leaving it there.
        QTimer::singleShot(0, qApp, &QCoreApplication::quit);
    }
}
