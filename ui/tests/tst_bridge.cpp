#include "AdapterTestSupport.h"

#include <QEventLoop>
#include <QPointer>
#include <QSignalSpy>
#include <QTest>
#include <QTimer>

using adapter_test::liveCounts;
using adapter_test::settledBaseline;
using adapter_test::spinUntil;
using adapter_test::spinUntilLiveCounts;

// M1.6: the waker -> queued drain path (ADR-0003 D5) and the error model
// (D6), proved without a window.
class TstBridge : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void theAbiVersionIsCheckedBeforeAnythingElse();
    void aDrainBudgetOfOneStillDeliversEveryEvent();
    void aFailingStatementSurfacesKindNativeCodeAndPosition();
    void aBlockedStatementDoesNotStallTheEventLoop();
    void aNestedEventLoopDuringADrainNeverReEntersIt();
    void deleteLaterFromInsideADrainTearsDownCleanly();
    void eventsWithNoOwnerAreReleasedAndCounted();
    void closingASessionDrainsTheUnsolicitedTerminalEventWithoutAsserting();
};

void TstBridge::theAbiVersionIsCheckedBeforeAnythingElse()
{
    QCOMPARE(reldex_abi_version(), static_cast<quint32>(RELDEX_ABI_VERSION));
    Bridge bridge;
    QVERIFY(bridge.isValid());
    QVERIFY(bridge.session() != nullptr);
    QVERIFY(bridge.metrics() != nullptr);
    QCOMPARE(bridge.drainEventBudget(), 256); // ADR-0003 D5
    QCOMPARE(bridge.drainTimeBudgetMs(), 4);
}

void TstBridge::aDrainBudgetOfOneStillDeliversEveryEvent()
{
    // The no-lost-wake proof. The waker is edge-triggered on empty ->
    // non-empty (reldex.h, "THE WAKER"), so a burst that arrives while the
    // queue is already non-empty produces exactly ONE wake. With a budget of
    // one event per drain, everything after the first event can only be
    // delivered by drain() re-posting itself (A16).
    constexpr int kFetches = 32;
    constexpr int kFetchRows = 10;

    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    session->setMockRows(100000);
    session->setFetchRows(kFetchRows);
    session->setMaxFetchesInFlight(kFetches);
    session->setAutoFetch(false);
    session->setRunOnOpen(true);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Fetching; }));

    for (int index = 0; index < kFetches; ++index) {
        session->fetchMoreRows();
    }
    QCOMPARE(session->fetchesInFlight(), kFetches);

    // Deliberately do NOT run the event loop: the queue must fill up while
    // nothing is draining it, so only one wake can have fired for the burst.
    QDeadlineTimer deadline(60000);
    while (bridge.pendingEvents() < kFetches && !deadline.hasExpired()) {
        QThread::yieldCurrentThread();
    }
    QCOMPARE(bridge.pendingEvents(), qint64(kFetches));

    bridge.setDrainEventBudget(1);
    bridge.setDrainTimeBudgetMs(0);

    QSignalSpy drains(&bridge, &Bridge::drained);
    QVERIFY(spinUntil([session] { return session->fetchesInFlight() == 0; }));
    QCOMPARE(session->rowsFetched(), qint64(kFetches) * kFetchRows);
    QCOMPARE(bridge.session()->model()->rowCount(), kFetches * kFetchRows);
    QCOMPARE(bridge.pendingEvents(), qint64(0));
    // One event per drain means at least one drain per event.
    QVERIFY(drains.count() >= kFetches);
}

void TstBridge::aFailingStatementSurfacesKindNativeCodeAndPosition()
{
    const auto baseline = settledBaseline();
    {
        Bridge bridge;
        QVERIFY(bridge.isValid());
        SessionController *session = bridge.session();
        QVERIFY(session->open());
        QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));

        QSignalSpy failures(session, &SessionController::failed);
        QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_FAILING));
        QVERIFY(spinUntil([session] { return session->state() == SessionController::Failed; }));

        QCOMPARE(failures.count(), 1);
        QVERIFY(session->hasError());
        QCOMPARE(session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_SYNTAX));
        QCOMPARE(session->errorNativeCode(), 942);
        QCOMPARE(session->errorLine(), 1);
        QCOMPARE(session->errorColumn(), 15);
        QVERIFY2(session->errorMessage().contains(QStringLiteral("does not exist")),
                 qPrintable(session->errorMessage()));
        QVERIFY2(session->errorNativeMessage().startsWith(QStringLiteral("ORA-00942")),
                 qPrintable(session->errorNativeMessage()));
    }
    // The error object was freed exactly once when the event's handle went out
    // of scope; what survives is the copied-out value data. ABI 3 can assert
    // that directly now -- `errors` counts objects held by the caller, queued
    // in an undrained event, *and* sitting in a thread's last-error slot, so a
    // failed statement that left one behind anywhere shows up here.
    QVERIFY2(spinUntilLiveCounts(baseline),
             qPrintable(QStringLiteral("live counts after an error path: %1 (baseline %2)")
                                .arg(liveCounts().toString(), baseline.toString())));
}

void TstBridge::aBlockedStatementDoesNotStallTheEventLoop()
{
    // ADR-0003 K6 in miniature. The mock's Block statement parks a worker
    // thread until it is released; `block_duration_ms = 0` means "until
    // released", so nothing here sleeps for ten seconds to make the point.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    session->setMockBlockDurationMs(0);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));

    int ticks = 0;
    QTimer timer;
    timer.setInterval(5);
    connect(&timer, &QTimer::timeout, this, [&ticks] { ++ticks; });
    timer.start();

    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_BLOCK));
    QCOMPARE(session->state(), SessionController::Executing);

    // Liveness, not latency: the loop keeps running while the statement is
    // outstanding. No upper bound is asserted anywhere.
    QVERIFY(spinUntil([&ticks] { return ticks >= 20; }));
    QCOMPARE(session->state(), SessionController::Executing);
    timer.stop();

    QVERIFY(bridge.releaseMockBlock(session->sessionId()));
    QVERIFY(spinUntil(
            [session] { return session->state() != SessionController::Executing; }));
    QVERIFY(!session->hasError());
}

void TstBridge::aNestedEventLoopDuringADrainNeverReEntersIt()
{
    // A modal dialog -- or any `QEventLoop::exec()` run from a slot a drain
    // reached -- delivers the posted drains that arrive while the outer drain
    // is still walking the queue. Re-entering would dispatch events inside an
    // outer dispatch: the model would be mutated mid-signal and the event
    // order the library guarantees (A5) would stop being the order the adapter
    // applies. `drain()` must bail out and re-post instead.
    constexpr int kRows = 20000;

    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    ResultTableModel *model = session->model();

    QSignalSpy drains(&bridge, &Bridge::drained);
    bool nestedLoopRan = false;
    int drainsCompletedInsideNestedLoop = -1;

    // Emitted from inside `SessionController::handleEvent()`, which is inside
    // `Bridge::drain()`.
    connect(session, &SessionController::rowsFetchedChanged, this, [&] {
        if (nestedLoopRan) {
            return;
        }
        nestedLoopRan = true;
        const int before = drains.count();
        QEventLoop nested;
        QTimer::singleShot(20, &nested, [&nested] { nested.quit(); });
        nested.exec();
        // A drain that nested would have run to completion here, and a
        // completed drain emits `drained`. None may.
        drainsCompletedInsideNestedLoop = drains.count() - before;
    });

    session->setMockRows(kRows);
    session->setFetchRows(200);
    session->setMaxFetchesInFlight(4);
    session->setRunOnOpen(true);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] {
        return session->state() == SessionController::ResultComplete
                || session->state() == SessionController::Failed;
    }));

    QVERIFY(nestedLoopRan);
    QCOMPARE(drainsCompletedInsideNestedLoop, 0);

    // And nothing was lost or double-applied on the way.
    QCOMPARE(session->state(), SessionController::ResultComplete);
    QCOMPARE(model->rowCount(), kRows);
    QCOMPARE(session->rowsFetched(), qint64(kRows));
    QCOMPARE(bridge.orphanEvents(), qint64(0));
    QCOMPARE(bridge.pendingEvents(), qint64(0));
    QCOMPARE(model->data(model->index(kRows - 1, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(kRows));
}

void TstBridge::deleteLaterFromInsideADrainTearsDownCleanly()
{
    // The other half of the same rule, stated in Bridge.h: destroy the Bridge
    // with `deleteLater()`, never `delete`, from anything a drain can reach.
    // `~Bridge` tears the hub down underneath the loop that is still walking
    // it; `deleteLater()` defers that to after the drain has returned.
    const auto baseline = settledBaseline();
    auto *bridge = new Bridge;
    QVERIFY(bridge->isValid());
    SessionController *session = bridge->session();
    session->setMockRows(500000);
    session->setFetchRows(500);
    session->setMaxFetchesInFlight(8);
    session->setRunOnOpen(true);

    QPointer<Bridge> alive(bridge);
    bool askedForDeletion = false;
    bool survivedTheDrainItAskedFrom = false;
    qint64 eventsStillQueued = -1;
    connect(session, &SessionController::rowsFetchedChanged, this, [&] {
        if (askedForDeletion) {
            return;
        }
        askedForDeletion = true;
        bridge->deleteLater();
        // Still inside the drain: the hub must still be there, or the loop
        // walking it is walking freed memory. This is the assertion `delete`
        // would fail and `deleteLater()` passes.
        survivedTheDrainItAskedFrom = !alive.isNull() && bridge->isValid();
        eventsStillQueued = bridge->pendingEvents();
    });

    QVERIFY(session->open());
    QVERIFY(spinUntil([&askedForDeletion] { return askedForDeletion; }));
    QVERIFY(survivedTheDrainItAskedFrom);
    QVERIFY(eventsStillQueued >= 0);

    // The deferred delete runs on the way back out to the event loop. Asked for
    // explicitly in case this loop level has not run it yet.
    QCoreApplication::sendPostedEvents(nullptr, QEvent::DeferredDelete);
    QVERIFY(alive.isNull());

    // Fetches were still in flight when the hub was destroyed; nothing may
    // call back into the freed object afterwards (D5 rule 2 + A10). Keep the
    // loop turning so a stray wake would have every chance to.
    QTest::qWait(100);

    // And nothing the destroyed Bridge held is still live -- including the
    // batches that were sitting in undrained events at the moment it died,
    // which ABI 3 counts until they are released.
    QVERIFY2(spinUntilLiveCounts(baseline),
             qPrintable(QStringLiteral("live counts after a deleteLater teardown: %1 "
                                       "(baseline %2)")
                                .arg(liveCounts().toString(), baseline.toString())));
}

void TstBridge::eventsWithNoOwnerAreReleasedAndCounted()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    session->setMockRows(10000);
    session->setFetchRows(100);
    session->setMaxFetchesInFlight(2);
    session->setRunOnOpen(true);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->rowsFetched() > 0; }));

    // Unroute the session while fetches are in flight: their events still
    // arrive, and the batches they carry must be released rather than leaked
    // or delivered to nobody silently.
    bridge.unregisterSession(session->sessionId());
    QVERIFY(spinUntil([&bridge] { return bridge.orphanEvents() > 0; }));
    QVERIFY(bridge.orphanEvents() > 0);
}

void TstBridge::closingASessionDrainsTheUnsolicitedTerminalEventWithoutAsserting()
{
    // Regression test: `TERMINAL` (ADR-0003 A28) is delivered request-less
    // (`request == 0`) right after `SESSION_CLOSED` whenever a close
    // actually ends the session, and request ids here start at 1
    // (SessionController.h). `handleEvent()` used to look `0` up in
    // `m_outstanding`, never find it, and hit
    // `Q_ASSERT_X(known, ...)` -- invisible under this project's default
    // RelWithDebInfo build (`Q_ASSERT_X` compiles to nothing there), so this
    // only reproduces built with `-DCMAKE_BUILD_TYPE=Debug`. This test does
    // not itself force that build type; it is the regression check to run
    // under one, and passes unconditionally now that the guard is fixed.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));

    QSignalSpy closed(session, &SessionController::sessionClosed);
    QVERIFY(session->closeSession());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Closed; }));
    QCOMPARE(closed.count(), 1);
}

QTEST_GUILESS_MAIN(TstBridge)

#include "tst_bridge.moc"
