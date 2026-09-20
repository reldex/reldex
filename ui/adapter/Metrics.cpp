#include "Metrics.h"

#include <QByteArray>
#include <QFile>
#include <QMutexLocker>
#include <QTextStream>
#include <QtQuick/QQuickWindow>

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
    if (m_enabled == enabled) {
        return;
    }
    m_enabled = enabled;
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

    const QMutexLocker locker(&m_frameMutex);
    m_frameIntervalsNs.clear();
    m_lastFrameNs = -1;
    m_firstFrameAfterInsertNs = -1;
    m_rowsInserted = false;
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

void Metrics::onFrameSwapped()
{
    if (!m_enabled) {
        return;
    }
    const qint64 now = m_clock.nsecsElapsed();
    const QMutexLocker locker(&m_frameMutex);
    if (m_rowsInserted && m_firstFrameAfterInsertNs < 0) {
        m_firstFrameAfterInsertNs = now;
    }
    if (m_lastFrameNs >= 0 && m_frameIntervalsNs.size() < kMaxSamples) {
        m_frameIntervalsNs.append(now - m_lastFrameNs);
    }
    m_lastFrameNs = now;
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
    for (const DrainSample &sample : m_drains) {
        out << "drain," << sample.atNs << ',' << sample.events << ',' << sample.nanos << '\n';
    }
    {
        const QMutexLocker locker(&m_frameMutex);
        out << "mark,firstFrameAfterInsertNs," << m_firstFrameAfterInsertNs << ",\n";
        for (const qint64 interval : m_frameIntervalsNs) {
            out << "frame," << interval << ",,\n";
        }
    }
    out.flush();
    return file.error() == QFileDevice::NoError;
}

qint64 Metrics::residentBytes()
{
#ifdef Q_OS_WIN
    PROCESS_MEMORY_COUNTERS counters {};
    counters.cb = sizeof counters;
    if (::K32GetProcessMemoryInfo(::GetCurrentProcess(), &counters, sizeof counters) != 0) {
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
