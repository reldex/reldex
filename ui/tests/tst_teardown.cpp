#include "AdapterTestSupport.h"

#include <Metrics.h>

#include <QElapsedTimer>
#include <QTest>

#include <memory>

using adapter_test::envNumber;
using adapter_test::spinUntil;

// M1.6 / spike criterion K5: destroy the C++ bridge under a flood of
// completions, 10,000 times, and neither hang, crash, nor leak.
//
// What makes it safe is stated where it is implemented (ui/adapter/Bridge.cpp,
// ~Bridge), and it is exactly ADR-0003 D5 rule 2 plus A10:
//
//   1. reldex_hub_set_waker(hub, NULL, NULL) -- does not return while a wake
//      is in progress, so the trampoline cannot see a dying Bridge;
//   2. the SessionController and its model die next, releasing every batch;
//   3. whatever is still queued is taken and released;
//   4. reldex_hub_destroy, with no other thread inside any reldex_* call --
//      this adapter never uses the cancel-from-any-thread allowance, so there
//      is no thread to join (A10);
//   5. ~QObject discards any drain this Bridge had posted for itself.
//
// Iteration count: `RELDEX_UI_TEARDOWN_ITERATIONS` overrides the default of
// 10,000 (CI may want fewer; a sanitizer build certainly does).
class TstTeardown : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void destroyingTheBridgeUnderAFloodOfCompletions();
    void destroyingTheBridgeWhileTheSessionIsStillConnecting();
};

void TstTeardown::destroyingTheBridgeUnderAFloodOfCompletions()
{
    const auto iterations = static_cast<int>(
            qBound<qint64>(1LL, envNumber("RELDEX_UI_TEARDOWN_ITERATIONS", 10000), 1000000LL));

    const qint64 rssBefore = Metrics::residentBytes();
    QElapsedTimer wall;
    wall.start();

    for (int iteration = 0; iteration < iterations; ++iteration) {
        auto bridge = std::make_unique<Bridge>();
        QVERIFY(bridge->isValid());
        SessionController *session = bridge->session();
        // Small fetches and many of them in flight: the point is a queue with
        // completions arriving while the Bridge is torn down.
        session->setMockRows(1000000);
        session->setFetchRows(4);
        session->setMaxFetchesInFlight(16);
        session->setRunOnOpen(true);
        QVERIFY(session->open());

        const bool flooding = spinUntil(
                [session] {
                    return session->rowsFetched() > 0
                            || session->state() == SessionController::Failed;
                },
                30000);
        QVERIFY2(flooding, "the flood never started");
        QVERIFY(session->state() != SessionController::Failed);

        // One more turn, so several fetches are outstanding and several
        // completions are queued behind the one just handled.
        QCoreApplication::processEvents(QEventLoop::AllEvents, 1);

        bridge.reset(); // ~Bridge, mid-flood
    }

    const qint64 elapsedMs = wall.elapsed();
    const qint64 rssAfter = Metrics::residentBytes();
    qInfo("K5: %d iterations in %lld ms (%.3f ms/iteration); RSS %lld -> %lld bytes (delta %lld)",
          iterations, static_cast<long long>(elapsedMs),
          iterations > 0 ? static_cast<double>(elapsedMs) / iterations : 0.0,
          static_cast<long long>(rssBefore), static_cast<long long>(rssAfter),
          static_cast<long long>(rssAfter - rssBefore));

    // A leaked batch per iteration would be hundreds of megabytes here. This
    // is a gross-leak guard, not a memory budget -- K3 is M1.8's to measure.
    if (rssBefore > 0 && rssAfter > 0) {
        QVERIFY2(rssAfter - rssBefore < 512LL * 1024 * 1024,
                 qPrintable(QStringLiteral("RSS grew by %1 bytes over %2 iterations")
                                    .arg(rssAfter - rssBefore)
                                    .arg(iterations)));
    }
}

void TstTeardown::destroyingTheBridgeWhileTheSessionIsStillConnecting()
{
    // The other teardown window: the OPENED event has not been drained yet,
    // so the pump is mid-connect when the waker is unregistered.
    const auto iterations = static_cast<int>(
            qBound<qint64>(1LL, envNumber("RELDEX_UI_TEARDOWN_CONNECT_ITERATIONS", 2000), 1000000LL));
    for (int iteration = 0; iteration < iterations; ++iteration) {
        auto bridge = std::make_unique<Bridge>();
        QVERIFY(bridge->isValid());
        SessionController *session = bridge->session();
        session->setMockRows(1000);
        session->setRunOnOpen(true);
        QVERIFY(session->open());
        bridge.reset();
    }
}

QTEST_GUILESS_MAIN(TstTeardown)

#include "tst_teardown.moc"
