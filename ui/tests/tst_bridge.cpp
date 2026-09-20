#include "AdapterTestSupport.h"

#include <QSignalSpy>
#include <QTest>
#include <QTimer>

using adapter_test::spinUntil;

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
    void eventsWithNoOwnerAreReleasedAndCounted();
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

    // The error object itself was freed exactly once when the event's handle
    // went out of scope; what survives is this copied-out value data. There is
    // no counter in reldex.h to assert that with, so the structural guarantee
    // (reldex::ErrorHandle) plus ASan is the evidence -- see ui/README.md.
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

QTEST_GUILESS_MAIN(TstBridge)

#include "tst_bridge.moc"
