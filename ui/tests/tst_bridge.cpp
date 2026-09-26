#include "AdapterTestSupport.h"

#include <QEventLoop>
#include <QPointer>
#include <QSignalSpy>
#include <QTest>
#include <QTimer>

#include <chrono>
#include <thread>

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
    void aLibraryOlderThanTheHeaderIsRefusedWithATypedError();
    void aDrainBudgetOfOneStillDeliversEveryEvent();
    void aFailingStatementSurfacesKindNativeCodeAndPosition();
    void aBlockedStatementDoesNotStallTheEventLoop();
    void aNestedEventLoopDuringADrainNeverReEntersIt();
    void deleteLaterFromInsideADrainTearsDownCleanly();
    void eventsWithNoOwnerAreReleasedAndCounted();
    void closingASessionDrainsTheUnsolicitedTerminalEventWithoutAsserting();
    void aSessionLostMidStatementEndsWithTerminalAndStopsCounting();
    void progressAndUnknownEventsNeverTouchTheRequestBookkeeping();
    void aCloseAnsweredAfterTerminalLeavesTheSessionFailed();
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
    QCOMPARE(bridge.startError(), Bridge::StartError::None);
}

void TstBridge::aLibraryOlderThanTheHeaderIsRefusedWithATypedError()
{
    // Field presence is decided by the ABI minor, not by `struct_size`: a
    // field a 3.1 library never fills can sit in its struct's tail padding
    // (ADR-0003 A38), so an adapter must not run against an older minor.
    constexpr quint32 major = RELDEX_ABI_VERSION_MAJOR;
    constexpr quint32 minor = RELDEX_ABI_VERSION_MINOR;
    static_assert(minor > 0, "this test needs a header minor to go below");
    QCOMPARE(Bridge::checkAbiVersion((major << 16) | minor), Bridge::StartError::None);
    QCOMPARE(Bridge::checkAbiVersion((major << 16) | (minor + 1)), Bridge::StartError::None);
    QCOMPARE(Bridge::checkAbiVersion((major << 16) | (minor - 1)),
             Bridge::StartError::AbiMinorTooOld);
    QCOMPARE(Bridge::checkAbiVersion(((major + 1) << 16) | minor),
             Bridge::StartError::AbiMajorMismatch);
    QCOMPARE(Bridge::checkAbiVersion(((major - 1) << 16) | (minor + 5)),
             Bridge::StartError::AbiMajorMismatch);
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

void TstBridge::aSessionLostMidStatementEndsWithTerminalAndStopsCounting()
{
    // ABI 3.2: a session lost mid-statement ends with TERMINAL at once --
    // nobody closed it -- and stops counting while the hub lives on.
    const auto baseline = settledBaseline();
    {
        Bridge bridge;
        QVERIFY(bridge.isValid());
        SessionController *session = bridge.session();
        QVERIFY(session->open());
        QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));
        QCOMPARE(liveCounts().sessions, baseline.sessions + 1);

        QSignalSpy transaction(session, &SessionController::transactionStateChanged);
        QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_DML));
        QVERIFY(spinUntil([session] {
            return session->state() == SessionController::Ready
                    && session->transactionPossiblyActive();
        }));
        QCOMPARE(transaction.count(), 1);

        QSignalSpy terminated(session, &SessionController::terminated);
        QSignalSpy failures(session, &SessionController::failed);
        QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_LOSE_SESSION));
        QVERIFY(spinUntil([&terminated] { return terminated.count() == 1; }));
        QCOMPARE(session->state(), SessionController::Failed);
        QCOMPARE(failures.count(), 1);
        QVERIFY(session->isTerminated());
        QVERIFY2(terminated.at(0).at(0).toBool(),
                 "the DML's transaction went with the connection, and TERMINAL says so");
        QVERIFY(!terminated.at(0).at(1).toBool());
        QVERIFY(session->transactionPossiblyLost());
        QCOMPARE(session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_NETWORK_LOST));
        QCOMPARE(session->errorNativeCode(), 3113);
        QCOMPARE(session->outstandingRequests(), 0);
        QCOMPARE(liveCounts().sessions, baseline.sessions);
        // The session ended, so no transaction is open any more -- the flag
        // says so, and says it once; whether one was lost is the TERMINAL's.
        QVERIFY(!session->transactionPossiblyActive());
        QCOMPARE(transaction.count(), 2);
        QCOMPARE(transaction.last().at(0).toBool(), false);
    }
    QVERIFY2(spinUntilLiveCounts(baseline),
             qPrintable(QStringLiteral("live counts after a lost session: %1 (baseline %2)")
                                .arg(liveCounts().toString(), baseline.toString())));
}

void TstBridge::progressAndUnknownEventsNeverTouchTheRequestBookkeeping()
{
    // EXECUTING names the statement it announces (in `executing_request`,
    // with `request == 0`); a kind no header defines may carry anything,
    // including a live request id. Neither may consume the entry the
    // statement's EXECUTED is owed (ABI 3.2; D7).
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    session->setMockBlockDurationMs(0);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));

    QSignalSpy drains(&bridge, &Bridge::drained);
    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_BLOCK));
    QCOMPARE(session->outstandingRequests(), 1);
    // The worker announces the statement, then blocks: EXECUTING is drained
    // while the EXECUTED is still owed.
    QVERIFY(spinUntil([&drains] {
        int events = 0;
        for (const QList<QVariant> &drain : std::as_const(drains)) {
            events += drain.at(0).toInt();
        }
        return events >= 1;
    }));
    QCOMPARE(session->outstandingRequests(), 1);

    for (quint64 request = 0; request < 8; ++request) {
        ReldexEvent future = reldex::makeEvent();
        future.kind = 9999;
        future.session = session->sessionId();
        future.request = request;
        session->handleEvent(future, reldex::BatchHandle(), reldex::ErrorHandle(),
                             reldex::LinesHandle());
    }
    QCOMPARE(session->outstandingRequests(), 1);
    QCOMPARE(session->state(), SessionController::Executing);

    QVERIFY(bridge.releaseMockBlock(session->sessionId()));
    QVERIFY(spinUntil(
            [session] { return session->state() != SessionController::Executing; }));
    QCOMPARE(session->outstandingRequests(), 0);
    QVERIFY(!session->hasError());
}

void TstBridge::aCloseAnsweredAfterTerminalLeavesTheSessionFailed()
{
    // A close submitted after the session was lost but before its TERMINAL
    // is drained is answered after that TERMINAL, as FAILED. It must not turn
    // the Failed the TERMINAL set into Closed.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->state() == SessionController::Ready; }));

    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_LOSE_SESSION));
    // Without spinning the event loop, so nothing is drained yet: a fresh
    // session's loss queues EXECUTING, TRANSACTION_STATE, EXECUTED and then
    // TERMINAL (the Rust suite pins the order), so four queued events mean
    // TERMINAL is among them.
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(60);
    while (bridge.pendingEvents() < 4) {
        QVERIFY2(std::chrono::steady_clock::now() < deadline, "the loss was never queued");
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
    }
    QSignalSpy closed(session, &SessionController::sessionClosed);
    QSignalSpy terminated(session, &SessionController::terminated);
    QSignalSpy failures(session, &SessionController::failed);
    QVERIFY(session->closeSession());

    QVERIFY(spinUntil([&closed] { return closed.count() == 1; }));
    QCOMPARE(terminated.count(), 1);
    QCOMPARE(closed.constFirst().at(0).toInt(), static_cast<int>(RELDEX_CLOSE_OUTCOME_FAILED));
    QCOMPARE(closed.constFirst().at(1).toBool(), false);
    QCOMPARE(session->state(), SessionController::Failed);
    QCOMPARE(failures.count(), 1);
    QVERIFY(session->isTerminated());
    QVERIFY(!session->transactionPossiblyActive());
    QCOMPARE(session->outstandingRequests(), 0);
}

QTEST_GUILESS_MAIN(TstBridge)

#include "tst_bridge.moc"
