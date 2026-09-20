#pragma once

// Instrumentation for spike S15's measurement run (M1.8), kept in one class so
// the measurement has a single place to read from and the rest of the adapter
// has a single place to call.
//
// Compiled in unconditionally and **off by default**: every recording entry
// point is an inline `if (!m_enabled) return;` in front of an out-of-line
// implementation, so a disabled build pays one predictable branch per drain
// and per frame and nothing else. Enable it with the `RELDEX_UI_METRICS`
// environment variable (any value but `0`), from QML, or from C++.
//
// M1.8 does the measuring; this only records. No thresholds, no verdicts, no
// judgement about what a number means -- those belong in the spike report.

#include <QElapsedTimer>
#include <QMutex>
#include <QObject>
#include <QString>
#include <QVariantMap>
#include <QVector>
#include <QtQml/qqmlregistration.h>
// Included rather than forward-declared: `attachWindow` is Q_INVOKABLE, and
// moc needs a complete type for every pointer parameter it registers.
#include <QtQuick/QQuickWindow>

#include <atomic>

class Metrics : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("Metrics is created by Bridge and reached as bridge.metrics")

    Q_PROPERTY(bool enabled READ isEnabled WRITE setEnabled NOTIFY enabledChanged)

public:
    explicit Metrics(QObject *parent = nullptr);
    ~Metrics() override;

    /// Read from the render thread as well as the GUI thread (`frameSwapped`
    /// is emitted on the render thread), so the flag is atomic rather than a
    /// plain `bool` whose torn or stale read would be a data race.
    [[nodiscard]] bool isEnabled() const noexcept
    {
        return m_enabled.load(std::memory_order_relaxed);
    }
    void setEnabled(bool enabled);

    // --- marks on the execute -> first pixels path -------------------------
    // Each is recorded once per run; `reset()` starts a new run.

    void markExecuteSubmitted() { if (isEnabled()) { recordMark(m_executeSubmittedNs); } }
    void markFirstEvent() { if (isEnabled()) { recordMark(m_firstEventNs); } }
    void markFirstRowsInserted() { if (isEnabled()) { recordFirstRowsInserted(); } }
    void markResultComplete(qint64 rows) { if (isEnabled()) { recordResultComplete(rows); } }

    /// One drain of the hub's event queue: how many events it took and how
    /// long it held the UI thread.
    void recordDrain(int events, qint64 nanos)
    {
        if (isEnabled()) {
            recordDrainImpl(events, nanos);
        }
    }

    /// Starts recording frame intervals from `window`'s swap signal.
    ///
    /// `frameSwapped` is emitted on the **render** thread, so the sample is
    /// taken there under a mutex rather than queued to the GUI thread, which
    /// would time the GUI thread's backlog instead of the frame.
    Q_INVOKABLE void attachWindow(QQuickWindow *window);

    /// Clears every sample and starts a new run.
    Q_INVOKABLE void reset();

    /// The marks and the aggregates, for a quick look from QML or a test.
    [[nodiscard]] Q_INVOKABLE QVariantMap summary() const;

    /// Writes every recorded sample as CSV: a `section,a,b,c` shape M1.8 can
    /// parse without a schema. Returns false if the file could not be written.
    Q_INVOKABLE bool writeCsv(const QString &path) const;

    /// Resident set size in bytes, or 0 where this platform is not handled.
    ///
    /// Static and always available -- K3 is a memory criterion and a test
    /// wants it whether or not metrics are enabled.
    ///
    /// Working set is *context*, not the verdict: it is what the OS currently
    /// keeps in RAM, so it moves with trimming and with pages the process
    /// shares. Judge K3 on [`privateBytes`].
    [[nodiscard]] Q_INVOKABLE static qint64 residentBytes();

    /// Private (committed, non-shared) bytes, or 0 where this platform is not
    /// handled.
    ///
    /// This is the number K3 should be judged on: it counts what this process
    /// actually made the system commit, which is what "RSS growth for 1M rows"
    /// is really asking about. Windows: `PROCESS_MEMORY_COUNTERS_EX::
    /// PrivateUsage`. Linux: `Private_Clean + Private_Dirty` from
    /// `/proc/self/smaps_rollup`, falling back to 0 when the kernel does not
    /// expose it. Elsewhere: 0, and said so rather than guessed.
    [[nodiscard]] Q_INVOKABLE static qint64 privateBytes();

    /// Nanoseconds since this object's clock started. Always available.
    [[nodiscard]] qint64 nowNs() const { return m_clock.nsecsElapsed(); }

Q_SIGNALS:
    void enabledChanged();

private:
    struct DrainSample
    {
        qint64 atNs;
        qint64 nanos;
        int events;
    };

    void recordMark(qint64 &slot);
    void recordFirstRowsInserted();
    void recordResultComplete(qint64 rows);
    void recordDrainImpl(int events, qint64 nanos);
    void onFrameSwapped();

    /// Bounded so a long run cannot grow without limit; K3 is about the data
    /// pipeline's memory, and the instrument must not be part of the answer.
    static constexpr int kMaxSamples = 200000;

    QElapsedTimer m_clock;
    std::atomic<bool> m_enabled { false };

    qint64 m_executeSubmittedNs = -1;
    qint64 m_firstEventNs = -1;
    qint64 m_firstRowsInsertedNs = -1;
    qint64 m_resultCompleteNs = -1;
    qint64 m_rowsStreamed = 0;

    QVector<DrainSample> m_drains;
    qint64 m_drainTotalNs = 0;
    qint64 m_drainTotalEvents = 0;

    mutable QMutex m_frameMutex;
    QVector<qint64> m_frameIntervalsNs;
    qint64 m_lastFrameNs = -1;
    qint64 m_firstFrameAfterInsertNs = -1;
    bool m_rowsInserted = false;
};
