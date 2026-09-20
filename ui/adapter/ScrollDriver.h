#pragma once

// `ScrollDriver` -- spike S15's measurement driver (M1.8).
//
// It exists because K1, K2 and K6 are questions about the *real* window on a
// real swapchain, and a human dragging a scrollbar cannot produce a repeatable
// number. It is **off unless an environment variable asks for it**: with
// `RELDEX_S15_SCROLL` and `RELDEX_S15_RUNS` unset, `attach()` returns
// immediately and this class connects nothing, allocates nothing and costs one
// `qgetenv` at startup.
//
// What it deliberately does NOT do, because a measurement driver that flatters
// the thing it measures is worse than no measurement:
//
//  * it does not touch the delegate, the model or the fetch path -- the grid
//    renders the same `Text` delegate over the same `ResultTableModel` it
//    renders for a human;
//  * it never reduces the row count. `RELDEX_S15_ROWS` is the mock's, and the
//    report states it;
//  * it skips no frame and drops no sample. Warm-up frames are discarded, and
//    only because the first frames of a phase include the phase change itself;
//  * it draws no verdicts. It records, aggregates (`Metrics`) and writes JSON;
//    the thresholds live in ADR-0003 and the judgement lives in the report.
//
// Patterns (`RELDEX_S15_SCROLL`, comma-separated, run in order):
//
//   flick   a flick-shaped velocity profile (initial velocity, Flickable's
//           1500 px/s^2 deceleration, re-kicked when it dies) applied to
//           `contentY` -- the row-per-frame rate a user's finger or wheel
//           actually produces. See `advance()` for why this is generated here
//           rather than by calling `Flickable::flick()`.
//   sweep   a constant `contentY` step per frame (`RELDEX_S15_SCROLL_STEP_PX`,
//           default 110 px ~ 5 rows/frame), reversing at the ends.
//   full    traverses the WHOLE result: contentY 0 -> max -> 0, with the step
//           sized so one down+up pass takes the phase's frame budget. At 1M
//           rows every frame lands in a different batch and a different
//           formatted window, so this is a cache-hostile pass, not a gentle
//           one.
//   jump    a uniformly random contentY every frame: the worst case for the
//           windowed format cache, since no frame can reuse the previous
//           frame's window.
//   blocksame / blockother
//           K6: the same `sweep` motion while a 10-second blocking statement
//           runs -- on this session (which, with one result per session,
//           necessarily replaces the result) or on a second Bridge/hub, which
//           is what "another session" looks like in the M1.6 adapter.
//   streambase / streamblocked
//           K6's "or any other session" clause: a *third* hub executes and
//           streams a result N times over while this window keeps sweeping,
//           first with nothing blocked and then with the second hub's session
//           blocked. The comparison of the two completion times is the
//           measurement; a single number from either alone is not.
//   streamscroll
//           scrolling *while* the 1M-row result is still streaming: the phase
//           re-executes on this session and then sweeps, so the timed frames
//           overlap the drains, `applyBatch` and `endInsertRows` instead of
//           following them. Reported next to the same phase's drain stats.
//   idle    no motion at all, for the baseline the K6 comparison needs.

#include <QJsonObject>
#include <QObject>
#include <QStringList>
#include <QVariantMap>
#include <QtQml/qqmlregistration.h>
// Included rather than forward-declared: both are Q_INVOKABLE parameter types
// and moc needs a complete type for each pointer it registers.
#include <QtQuick/QQuickItem>
#include <QtQuick/QQuickWindow>

class Bridge;

class ScrollDriver : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ScrollDriver is created by Bridge and reached as bridge.scrollDriver")

    /// True when the environment asked for a measurement run. `Main.qml` calls
    /// `attach()` unconditionally; everything after this flag is a no-op.
    Q_PROPERTY(bool enabled READ isEnabled CONSTANT)

public:
    explicit ScrollDriver(Bridge *bridge, QObject *parent = nullptr);
    ~ScrollDriver() override;

    [[nodiscard]] bool isEnabled() const noexcept { return m_enabled; }

    /// True when the driver, not `Bridge::autoStart()`, decides when the first
    /// query runs -- so that the "before" memory sample is taken on an app
    /// that has already drawn.
    [[nodiscard]] bool ownsStart() const noexcept { return m_ownsStart; }

    /// Binds the driver to the window whose frames are being timed and the
    /// `Flickable` (a `TableView` is one) it scrolls. A no-op when disabled.
    Q_INVOKABLE void attach(QQuickWindow *window, QQuickItem *flickable);

private Q_SLOTS:
    void onFirstFrameAfterInsert();
    void onResultComplete();
    void onAfterAnimating();
    void onStreamComplete();

private:
    enum class Pattern {
        Idle,
        Flick,
        Sweep,
        Full,
        Jump,
        BlockSame,
        BlockOther,
        StreamBase,
        StreamBlocked,
        StreamScroll,
    };

    [[nodiscard]] static bool isStreamPhase(Pattern pattern)
    {
        return pattern == Pattern::StreamBase || pattern == Pattern::StreamBlocked;
    }

    struct Phase
    {
        QString name;
        Pattern pattern = Pattern::Idle;
    };

    void startNextRun();
    void startFirstRun();
    void beginPhases();
    void beginPhase();
    void endPhase();
    void finish();

    void advance();
    /// Brings the measured window to the front. An occluded window is
    /// throttled by the compositor, which would make every frame number a
    /// measurement of the window manager rather than of this application.
    void raiseWindow();
    /// Asks Windows not to blank the display for the duration of the run.
    /// Process-local and undone in the destructor; it does **not** wake a
    /// display that is already off.
    void requestDisplayStaysOn();
    void submitBlock(Pattern pattern);
    void releaseBlock();
    /// Starts one timed execute-and-stream on the third hub.
    void startStreamRun();
    [[nodiscard]] double maxContentY() const;
    [[nodiscard]] double contentY() const;
    void setContentY(double y);

    [[nodiscard]] QVariantMap environmentReport() const;

    Bridge *m_bridge = nullptr;
    QQuickWindow *m_window = nullptr;
    QQuickItem *m_flickable = nullptr;
    /// The second hub, created only for the `blockother` phase and destroyed
    /// with this object. A second `Bridge` is what "a second session" means in
    /// the M1.6 adapter, which creates exactly one `SessionController` per
    /// `Bridge` (ui/README.md "Known limitations").
    Bridge *m_otherBridge = nullptr;
    /// The third hub: the session whose execute-to-`resultComplete` time is
    /// compared with and without [`m_otherBridge`]'s session blocked. It is a
    /// third *Bridge* because the M1.6 adapter creates exactly one
    /// `SessionController` per `Bridge`, so "two sessions, one of them blocked,
    /// plus the window's own" cannot be arranged any other way here. The
    /// same-hub version of the question is therefore not reachable, and the
    /// report says so rather than implying this covers it.
    Bridge *m_streamBridge = nullptr;

    bool m_enabled = false;
    bool m_attached = false;
    bool m_finished = false;

    // --- configuration, all from the environment --------------------------
    QList<Phase> m_phases;
    QString m_outPath;
    QString m_csvPath;
    QString m_label;
    int m_runs = 1;
    int m_phaseFrames = 1800;
    int m_warmupFrames = 30;
    double m_stepPx = 110.0;
    /// Initial flick velocity in px/s, and the deceleration and cut-off that
    /// shape its decay. The deceleration is `Flickable::flickDeceleration`'s
    /// own default, so the profile matches the one the view would produce.
    double m_flickVelocity = 9000.0;
    double m_flickVelocityNow = 0.0;
    static constexpr double kFlickDeceleration = 1500.0;
    static constexpr double kFlickMinVelocity = 50.0;
    /// The display's refresh rate, read from the window's screen at phase
    /// start; the flick profile advances by velocity x one frame of it.
    double m_refreshHz = 60.0;
    int m_settleMs = 2000;
    int m_blockMs = 0;
    bool m_quitWhenDone = true;
    bool m_topmost = true;
    bool m_topmostSet = false;
    bool m_keepDisplayAwake = true;
    bool m_displayRequested = false;
    int m_startDelayMs = 1500;
    bool m_ownsStart = false;
    /// How many execute-and-stream rounds a `streambase`/`streamblocked` phase
    /// times, and how many rows each one carries. Small enough that several
    /// rounds fit in a phase; the number that matters is the *difference*
    /// between the two phases, so both use the same values.
    int m_streamRuns = 5;
    qint64 m_streamRows = 100000;

    // --- run state ---------------------------------------------------------
    int m_runsDone = 0;
    int m_phaseIndex = -1;
    int m_frameIndex = 0;
    int m_direction = 1;
    double m_phaseMaxY = 0.0;
    double m_phaseStepPx = 0.0;
    double m_travelledPx = 0.0;
    qint64 m_phaseStartNs = 0;
    quint64 m_jumpState = 0x243F6A8885A308D3ULL;
    bool m_blockSubmitted = false;
    qint64 m_streamStartNs = 0;
    int m_streamDone = 0;
    QVariantList m_streamNs;

    QVariantList m_phaseResults;
    /// The drain / batch aggregates for the initial 1M-row stream, taken before
    /// the phases clear them so each phase can report its own.
    QVariantMap m_streamPhaseStats;
    QVariantMap m_memory;
    qint64 m_baselineResidentBytes = 0;
    qint64 m_baselinePrivateBytes = 0;
    /// Sampled when the app is idle and has drawn, immediately before the
    /// first execute. This is the "before" K3 should be read against.
    qint64 m_preRunResidentBytes = 0;
    qint64 m_preRunPrivateBytes = 0;
};
