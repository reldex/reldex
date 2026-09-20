#include "Metrics.h"

#include <QByteArray>
// Explicit, not via QByteArray: the only uses are inside the Linux branches
// below, so a missing include here would compile on this machine and fail on
// the first CI run that is not Windows.
#include <QByteArrayList>
#include <QFile>
#include <QMutexLocker>
#include <QTextStream>
#include <QThread>
#include <QtQuick/QQuickWindow>

#include <algorithm>

#ifdef Q_OS_WIN
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
// psapi.h must follow windows.h.
#include <psapi.h>
#elif defined(Q_OS_LINUX)
#include <unistd.h>
#endif

namespace {

bool metricsEnabledByEnvironment()
{
    const QByteArray value = qgetenv("RELDEX_UI_METRICS");
    return !value.isEmpty() && value != "0";
}

/// Nearest-rank percentile over an already-sorted vector. Nearest-rank rather
/// than interpolated because these are latencies: a reported p99 should be a
/// frame that actually happened, not an average of two that did.
qint64 percentileOf(const QVector<qint64> &sorted, double fraction)
{
    if (sorted.isEmpty()) {
        return -1;
    }
    auto rank = static_cast<qsizetype>(fraction * static_cast<double>(sorted.size()));
    rank = std::clamp<qsizetype>(rank, 0, sorted.size() - 1);
    return sorted.at(rank);
}

/// count / min / mean / percentiles / max, plus how many samples sat above a
/// few fixed millisecond marks. The marks are *counts*, deliberately not
/// judgements: 8 ms and 16.7 ms are the two numbers K1 is written in terms of,
/// 33 ms is K6's, and 20 ms is here because it is the next refresh period up
/// at 50 Hz and makes a near-miss visible.
QVariantMap statsOf(QVector<qint64> samples)
{
    QVariantMap map;
    map.insert(QStringLiteral("count"), static_cast<qint64>(samples.size()));
    if (samples.isEmpty()) {
        return map;
    }
    std::sort(samples.begin(), samples.end());
    qint64 total = 0;
    qint64 over8 = 0;
    qint64 over16_7 = 0;
    qint64 over20 = 0;
    qint64 over33 = 0;
    for (const qint64 sample : samples) {
        total += sample;
        if (sample > 8000000) {
            ++over8;
        }
        if (sample > 16700000) {
            ++over16_7;
        }
        if (sample > 20000000) {
            ++over20;
        }
        if (sample > 33000000) {
            ++over33;
        }
    }
    map.insert(QStringLiteral("minNs"), samples.first());
    map.insert(QStringLiteral("meanNs"), total / samples.size());
    map.insert(QStringLiteral("p50Ns"), percentileOf(samples, 0.50));
    map.insert(QStringLiteral("p90Ns"), percentileOf(samples, 0.90));
    map.insert(QStringLiteral("p95Ns"), percentileOf(samples, 0.95));
    map.insert(QStringLiteral("p99Ns"), percentileOf(samples, 0.99));
    map.insert(QStringLiteral("maxNs"), samples.last());
    map.insert(QStringLiteral("over8ms"), over8);
    map.insert(QStringLiteral("over16_7ms"), over16_7);
    map.insert(QStringLiteral("over20ms"), over20);
    map.insert(QStringLiteral("over33ms"), over33);
    return map;
}

} // namespace

Metrics::Metrics(QObject *parent)
    : QObject(parent)
    , m_enabled(metricsEnabledByEnvironment())
{
    m_clock.start();
}

Metrics::~Metrics() = default;

void Metrics::setEnabled(bool enabled)
{
    if (m_enabled.exchange(enabled, std::memory_order_relaxed) == enabled) {
        return;
    }
    Q_EMIT enabledChanged();
}

void Metrics::attachWindow(QQuickWindow *window)
{
    if (window == nullptr) {
        return;
    }
    // Direct connection on purpose: the slot runs on the render thread, which
    // is where the frame actually was.
    connect(window, &QQuickWindow::frameSwapped, this, &Metrics::onFrameSwapped,
            Qt::DirectConnection);
    // `beforeSynchronizing` .. `afterRendering`, NOT
    // `beforeFrameBegin` .. `afterFrameEnd`. The obvious pair is the wrong
    // one, and measurement rather than reasoning settled it: `QRhi::beginFrame`
    // -- which contains the swapchain's frame-latency wait -- runs *inside*
    // `beforeFrameBegin`/`afterFrameEnd`, so on a throttled desktop that pair
    // reported 252 ms per frame while the scene graph's own work was 0.3 ms.
    // This pair brackets the scene graph's CPU work for the frame (sync plus
    // the render pass) and excludes both the wait and the present.
    connect(window, &QQuickWindow::beforeSynchronizing, this, &Metrics::onFrameBegin,
            Qt::DirectConnection);
    connect(window, &QQuickWindow::afterRendering, this, &Metrics::onFrameEnd,
            Qt::DirectConnection);
}

void Metrics::reset()
{
    m_executeSubmittedNs = -1;
    m_firstEventNs = -1;
    m_firstRowsInsertedNs = -1;
    m_resultCompleteNs = -1;
    m_rowsStreamed = 0;
    m_drains.clear();
    m_drainTotalNs = 0;
    m_drainTotalEvents = 0;
    m_applyBatchNs.clear();
    m_runs.clear();

    const QMutexLocker locker(&m_frameMutex);
    m_frameIntervalsNs.clear();
    m_renderWorkNs.clear();
    m_frameBeginNs = -1;
    m_lastFrameNs = -1;
    m_firstFrameAfterInsertNs = -1;
    m_rowsInserted = false;
}

void Metrics::clearFrames()
{
    const QMutexLocker locker(&m_frameMutex);
    m_frameIntervalsNs.clear();
    m_renderWorkNs.clear();
    // Not `m_lastFrameNs`: clearing it would drop the interval that spans the
    // clear, which is the one frame a phase boundary actually costs. Keeping
    // it means the first recorded interval of a phase is measured from the
    // last frame of the previous one -- which is the truth about that frame.
    m_frameBeginNs = -1;
}

void Metrics::endRun()
{
    QVariantMap run;
    run.insert(QStringLiteral("executeSubmittedNs"), m_executeSubmittedNs);
    run.insert(QStringLiteral("firstEventNs"), m_firstEventNs);
    run.insert(QStringLiteral("firstRowsInsertedNs"), m_firstRowsInsertedNs);
    {
        const QMutexLocker locker(&m_frameMutex);
        run.insert(QStringLiteral("firstFrameAfterInsertNs"), m_firstFrameAfterInsertNs);
        m_firstFrameAfterInsertNs = -1;
        m_rowsInserted = false;
    }
    m_runs.append(run);

    m_executeSubmittedNs = -1;
    m_firstEventNs = -1;
    m_firstRowsInsertedNs = -1;
    m_resultCompleteNs = -1;
}

QVariantList Metrics::runs() const
{
    return m_runs;
}

void Metrics::recordMark(qint64 &slot)
{
    if (slot < 0) {
        slot = m_clock.nsecsElapsed();
    }
}

void Metrics::recordFirstRowsInserted()
{
    recordMark(m_firstRowsInsertedNs);
    const QMutexLocker locker(&m_frameMutex);
    m_rowsInserted = true;
}

void Metrics::recordResultComplete(qint64 rows)
{
    m_rowsStreamed = rows;
    recordMark(m_resultCompleteNs);
}

void Metrics::recordDrainImpl(int events, qint64 nanos)
{
    m_drainTotalNs += nanos;
    m_drainTotalEvents += events;
    if (m_drains.size() < kMaxSamples) {
        m_drains.append(DrainSample { m_clock.nsecsElapsed(), nanos, events });
    }
}

void Metrics::recordApplyBatchImpl(qint64 nanos)
{
    if (m_applyBatchNs.size() < kMaxSamples) {
        m_applyBatchNs.append(nanos);
    }
}

void Metrics::onFrameSwapped()
{
    // Runs on the render thread (DirectConnection), which is why `m_enabled`
    // is atomic and every sample below is taken under `m_frameMutex`.
    if (!isEnabled()) {
        return;
    }
    const qint64 now = m_clock.nsecsElapsed();
    bool firstFrame = false;
    {
        const QMutexLocker locker(&m_frameMutex);
        m_framesOnGuiThread = QThread::currentThread() == thread();
        if (m_rowsInserted && m_firstFrameAfterInsertNs < 0) {
            m_firstFrameAfterInsertNs = now;
            firstFrame = true;
        }
        if (m_lastFrameNs >= 0 && m_frameIntervalsNs.size() < kMaxSamples) {
            m_frameIntervalsNs.append(now - m_lastFrameNs);
        }
        m_lastFrameNs = now;
    }
    if (firstFrame) {
        // Outside the lock: a slot connected to this must never be able to
        // deadlock against the recorder, and across threads this is queued
        // anyway.
        Q_EMIT firstFrameAfterInsert();
    }
}

void Metrics::onFrameBegin()
{
    if (!isEnabled()) {
        return;
    }
    const qint64 now = m_clock.nsecsElapsed();
    const QMutexLocker locker(&m_frameMutex);
    m_frameBeginNs = now;
}

void Metrics::onFrameEnd()
{
    if (!isEnabled()) {
        return;
    }
    const qint64 now = m_clock.nsecsElapsed();
    const QMutexLocker locker(&m_frameMutex);
    if (m_frameBeginNs >= 0 && m_renderWorkNs.size() < kMaxSamples) {
        m_renderWorkNs.append(now - m_frameBeginNs);
    }
    m_frameBeginNs = -1;
}

bool Metrics::framesOnGuiThread() const
{
    const QMutexLocker locker(&m_frameMutex);
    return m_framesOnGuiThread;
}

QVariantMap Metrics::frameStats() const
{
    const QMutexLocker locker(&m_frameMutex);
    return statsOf(m_frameIntervalsNs);
}

QVariantMap Metrics::renderWorkStats() const
{
    const QMutexLocker locker(&m_frameMutex);
    return statsOf(m_renderWorkNs);
}

QVariantMap Metrics::applyBatchStats() const
{
    return statsOf(m_applyBatchNs);
}

QVariantMap Metrics::drainStats() const
{
    QVector<qint64> nanos;
    nanos.reserve(m_drains.size());
    for (const DrainSample &sample : m_drains) {
        nanos.append(sample.nanos);
    }
    QVariantMap map = statsOf(std::move(nanos));
    map.insert(QStringLiteral("totalEvents"), m_drainTotalEvents);
    return map;
}

QVariantMap Metrics::summary() const
{
    QVariantMap map;
    map.insert(QStringLiteral("executeSubmittedNs"), m_executeSubmittedNs);
    map.insert(QStringLiteral("firstEventNs"), m_firstEventNs);
    map.insert(QStringLiteral("firstRowsInsertedNs"), m_firstRowsInsertedNs);
    map.insert(QStringLiteral("resultCompleteNs"), m_resultCompleteNs);
    map.insert(QStringLiteral("rowsStreamed"), m_rowsStreamed);
    map.insert(QStringLiteral("drainCount"), static_cast<qint64>(m_drains.size()));
    map.insert(QStringLiteral("drainTotalNs"), m_drainTotalNs);
    map.insert(QStringLiteral("drainTotalEvents"), m_drainTotalEvents);
    map.insert(QStringLiteral("residentBytes"), residentBytes());
    map.insert(QStringLiteral("privateBytes"), privateBytes());

    const QMutexLocker locker(&m_frameMutex);
    map.insert(QStringLiteral("firstFrameAfterInsertNs"), m_firstFrameAfterInsertNs);
    map.insert(QStringLiteral("frameIntervalCount"),
               static_cast<qint64>(m_frameIntervalsNs.size()));
    return map;
}

bool Metrics::writeCsv(const QString &path) const
{
    QFile file(path);
    if (!file.open(QIODevice::WriteOnly | QIODevice::Truncate | QIODevice::Text)) {
        return false;
    }
    QTextStream out(&file);
    out << "section,a,b,c\n";
    out << "mark,executeSubmittedNs," << m_executeSubmittedNs << ",\n";
    out << "mark,firstEventNs," << m_firstEventNs << ",\n";
    out << "mark,firstRowsInsertedNs," << m_firstRowsInsertedNs << ",\n";
    out << "mark,resultCompleteNs," << m_resultCompleteNs << ",\n";
    out << "mark,rowsStreamed," << m_rowsStreamed << ",\n";
    out << "mark,residentBytes," << residentBytes() << ",\n";
    out << "mark,privateBytes," << privateBytes() << ",\n";
    for (const DrainSample &sample : m_drains) {
        out << "drain," << sample.atNs << ',' << sample.events << ',' << sample.nanos << '\n';
    }
    for (const qint64 nanos : m_applyBatchNs) {
        out << "applybatch," << nanos << ",,\n";
    }
    {
        const QMutexLocker locker(&m_frameMutex);
        out << "mark,firstFrameAfterInsertNs," << m_firstFrameAfterInsertNs << ",\n";
        for (const qint64 interval : m_frameIntervalsNs) {
            out << "frame," << interval << ",,\n";
        }
        for (const qint64 nanos : m_renderWorkNs) {
            out << "renderwork," << nanos << ",,\n";
        }
    }
    out.flush();
    return file.error() == QFileDevice::NoError;
}

qint64 Metrics::residentBytes()
{
#ifdef Q_OS_WIN
    PROCESS_MEMORY_COUNTERS_EX counters {};
    counters.cb = sizeof counters;
    if (::K32GetProcessMemoryInfo(::GetCurrentProcess(),
                                  reinterpret_cast<PROCESS_MEMORY_COUNTERS *>(&counters),
                                  sizeof counters)
        != 0) {
        return static_cast<qint64>(counters.WorkingSetSize);
    }
    return 0;
#elif defined(Q_OS_LINUX)
    QFile statm(QStringLiteral("/proc/self/statm"));
    if (!statm.open(QIODevice::ReadOnly | QIODevice::Text)) {
        return 0;
    }
    const QByteArrayList fields = statm.readAll().simplified().split(' ');
    if (fields.size() < 2) {
        return 0;
    }
    bool ok = false;
    const qint64 pages = fields.at(1).toLongLong(&ok);
    return ok ? pages * static_cast<qint64>(::sysconf(_SC_PAGESIZE)) : 0;
#else
    // Not handled here rather than guessed at; M1.8 measures on the dev
    // machine (Windows) and CI runs the correctness half only (ADR-0003 D10).
    return 0;
#endif
}

qint64 Metrics::privateBytes()
{
#ifdef Q_OS_WIN
    PROCESS_MEMORY_COUNTERS_EX counters {};
    counters.cb = sizeof counters;
    if (::K32GetProcessMemoryInfo(::GetCurrentProcess(),
                                  reinterpret_cast<PROCESS_MEMORY_COUNTERS *>(&counters),
                                  sizeof counters)
        != 0) {
        return static_cast<qint64>(counters.PrivateUsage);
    }
    return 0;
#elif defined(Q_OS_LINUX)
    // smaps_rollup is the cheap whole-process roll-up; it needs a 4.14+
    // kernel, so a missing file is a documented 0 rather than a slow walk of
    // /proc/self/smaps.
    QFile rollup(QStringLiteral("/proc/self/smaps_rollup"));
    if (!rollup.open(QIODevice::ReadOnly | QIODevice::Text)) {
        return 0;
    }
    qint64 privateKb = 0;
    bool sawAny = false;
    while (!rollup.atEnd()) {
        const QByteArray line = rollup.readLine();
        if (!line.startsWith("Private_Clean:") && !line.startsWith("Private_Dirty:")) {
            continue;
        }
        const QByteArrayList fields = line.simplified().split(' ');
        if (fields.size() < 2) {
            continue;
        }
        bool ok = false;
        const qint64 value = fields.at(1).toLongLong(&ok);
        if (ok) {
            privateKb += value;
            sawAny = true;
        }
    }
    return sawAny ? privateKb * 1024 : 0;
#else
    // macOS' nearest equivalent is TASK_VM_INFO's phys_footprint, which needs
    // mach headers and a device nobody here has to verify it on. Reported as
    // "not available" rather than as a number that might mean something else.
    return 0;
#endif
}
