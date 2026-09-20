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
#include <QVariantList>
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

    /// One drain of the hub's event queue: how many events it took, how long
    /// it held the UI thread, and how much of that was spent *inside* the
    /// boundary (`reldex_hub_next_event` plus taking ownership of what the
    /// event carried). The remainder is Qt model/view work — routing,
    /// `applyBatch`, `endInsertRows` and the signals it emits — which is the
    /// distinction spike S15's K4 turns on.
    void recordDrain(int events, qint64 nanos, qint64 boundaryNanos)
    {
        if (isEnabled()) {
            recordDrainImpl(events, nanos, boundaryNanos);
        }
    }

    /// One `ResultTableModel::applyBatch()` call: spike S15's K4 measures
    /// exactly this -- event delivered until the batch is described and its
    /// columns are viewable.
    void recordApplyBatch(qint64 nanos)
    {
        if (isEnabled()) {
            recordApplyBatchImpl(nanos);
        }
    }

    /// Starts recording frame intervals from `window`'s swap signal, and the
    /// per-frame scene-graph CPU work from its
    /// `beforeSynchronizing`/`afterRendering` pair.
    ///
    /// `frameSwapped`, `beforeSynchronizing`, `afterSynchronizing` and
    /// `afterRendering` are emitted on the **render** thread (with the threaded
    /// render loop), so those samples are taken there under a mutex rather than
    /// queued to the GUI thread, which would time the GUI thread's backlog
    /// instead of the frame. `afterAnimating` is emitted on the GUI thread; the
    /// same mutex covers it.
    ///
    /// Why more than one bracket: with vsync on, a swap interval quantizes to
    /// the refresh period, so it answers "did this frame make its budget?" and
    /// cannot answer "how much work was this frame?".
    /// `afterRendering - beforeSynchronizing` answers the second for the render
    /// thread, and [`guiFrameStats`] answers it for the GUI thread, which is
    /// where the view's `data()` calls live. None of them is a verdict; the
    /// spike report says which number it judges against which threshold.
    Q_INVOKABLE void attachWindow(QQuickWindow *window);

    /// Clears every sample and starts a new run.
    Q_INVOKABLE void reset();

    /// Clears the frame-interval and render-work samples only, leaving the
    /// marks, the drains and the batch costs alone. A measurement phase that
    /// wants to discard its own warm-up frames uses this.
    Q_INVOKABLE void clearFrames();

    /// Clears the drain and per-batch samples only, so a measurement phase can
    /// report the drains that happened *during it* rather than since the
    /// process started.
    Q_INVOKABLE void clearDrains();

    /// Ends the current execute -> first-pixels run: appends its four marks to
    /// [`runs`] and re-arms them for the next execute. Frame, drain and batch
    /// samples are untouched.
    Q_INVOKABLE void endRun();

    /// One entry per [`endRun`], each a map of the four marks in nanoseconds.
    [[nodiscard]] Q_INVOKABLE QVariantList runs() const;

    /// The marks and the aggregates, for a quick look from QML or a test.
    [[nodiscard]] Q_INVOKABLE QVariantMap summary() const;

    /// count / min / mean / p50 / p90 / p95 / p99 / max in nanoseconds, plus
    /// the number of samples above a few fixed millisecond marks. Aggregates,
    /// not verdicts: nothing here knows what a threshold is.
    [[nodiscard]] Q_INVOKABLE QVariantMap frameStats() const;
    [[nodiscard]] Q_INVOKABLE QVariantMap renderWorkStats() const;
    [[nodiscard]] Q_INVOKABLE QVariantMap applyBatchStats() const;
    [[nodiscard]] Q_INVOKABLE QVariantMap drainStats() const;
    [[nodiscard]] Q_INVOKABLE QVariantMap drainBoundaryStats() const;

    /// The GUI thread's share of a frame: from the previous frame's swap to
    /// the end of this frame's polish (`afterAnimating`).
    ///
    /// This is where `TableView` loads and reuses delegates and where every
    /// `data()` call — and therefore the windowed bulk formatter — actually
    /// runs, so without it a "per-frame cost" is only the render thread's half.
    /// **It also contains whatever the platform's update-request idle wait is**
    /// (`QT_QPA_UPDATE_IDLE_TIME`, 5 ms by default on this platform), so it is
    /// only a cost when that wait is taken out of the way.
    [[nodiscard]] Q_INVOKABLE QVariantMap guiFrameStats() const;
    /// `beforeSynchronizing - afterAnimating`: the handover from the GUI
    /// thread to the render thread. Not work; recorded so that the three
    /// brackets account for the whole frame rather than most of it.
    [[nodiscard]] Q_INVOKABLE QVariantMap guiHandoverStats() const;
    /// `afterSynchronizing - beforeSynchronizing`: the sync, during which the
    /// GUI thread is blocked and `updatePaintNode` runs.
    [[nodiscard]] Q_INVOKABLE QVariantMap syncStats() const;

    /// True when `frameSwapped` was last delivered on this object's own
    /// thread, i.e. the scene graph is using a non-threaded render loop.
    /// `false` before the first frame is meaningless, so the summary reports
    /// the frame count next to it.
    [[nodiscard]] Q_INVOKABLE bool framesOnGuiThread() const;

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

    /// Emitted once per run, from the thread that swapped the frame, when the
    /// first frame after the first inserted rows has been presented -- spike
    /// S15's K2 end point. A measurement driver connects to it queued (the
    /// default across threads) and must not do work in the slot beyond
    /// recording.
    void firstFrameAfterInsert();

private:
    struct DrainSample
    {
        qint64 atNs;
        qint64 nanos;
        qint64 boundaryNanos;
        int events;
    };

    void recordMark(qint64 &slot);
    void recordFirstRowsInserted();
    void recordResultComplete(qint64 rows);
    void recordDrainImpl(int events, qint64 nanos, qint64 boundaryNanos);
    void recordApplyBatchImpl(qint64 nanos);
    void onFrameSwapped();
    void onFrameBegin();
    void onFrameEnd();
    void onAfterAnimating();
    void onAfterSynchronizing();

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

    QVector<qint64> m_applyBatchNs;

    QVariantList m_runs;

    mutable QMutex m_frameMutex;
    QVector<qint64> m_frameIntervalsNs;
    /// `afterRendering - beforeSynchronizing`: the scene graph's own CPU cost
    /// for the frame, with neither the swapchain wait nor the present in it.
    QVector<qint64> m_renderWorkNs;
    /// Previous swap -> this frame's `afterAnimating`: the GUI thread's share.
    QVector<qint64> m_guiFrameNs;
    /// `afterAnimating` -> `beforeSynchronizing`: the handover.
    QVector<qint64> m_guiHandoverNs;
    /// `beforeSynchronizing` -> `afterSynchronizing`: the sync.
    QVector<qint64> m_syncNs;
    qint64 m_frameBeginNs = -1;
    qint64 m_afterAnimatingNs = -1;
    qint64 m_lastFrameNs = -1;
    qint64 m_firstFrameAfterInsertNs = -1;
    bool m_rowsInserted = false;
    /// Set by the first `afterSynchronizing` **after** rows were inserted, so
    /// the frame credited to K2 is one whose sync could actually have carried
    /// those rows. Without it the next `frameSwapped` is latched, and with the
    /// threaded render loop that swap can belong to a frame synchronized
    /// *before* the insert — i.e. K2 could be reported one frame early.
    bool m_syncedAfterInsert = false;
    bool m_framesOnGuiThread = false;
};
