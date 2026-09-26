#include "AdapterTestSupport.h"

#include <Bridge.h>
#include <ServerOutputController.h>
#include <ServerOutputModel.h>
#include <SessionController.h>
#include <SettingsController.h>

#include <QByteArray>
#include <QElapsedTimer>
#include <QSignalSpy>
#include <QTest>
#include <QThread>

using adapter_test::spinUntil;

namespace {

/// A fresh `WorksheetId`, the same call `Bridge`'s own constructor makes for
/// `bridge.serverOutput`. Pure, no workspace needed (`reldex.h`'s own doc
/// comment on `reldex_workspace_new_worksheet_id`).
QByteArray newWorksheetId()
{
    QByteArray id(16, Qt::Uninitialized);
    reldex_workspace_new_worksheet_id(reinterpret_cast<std::uint8_t *>(id.data()));
    return id;
}

} // namespace

// M4.7: the DBMS_OUTPUT pane's adapter-level behavior --
// `ServerOutputController` (settings round-trip/provenance, per-worksheet
// isolation, truncation/invalid-UTF-8/read-failure reporting, arrival order,
// clear) and `ServerOutputModel` (the 100,000-line virtualization
// measurement; the QML-level "no per-line item" proof lives in
// `tst_coreinfo.cpp`, which is the one binary in this suite that can load a
// real `ListView`).
//
// Every test here shares `AdapterTestSupport.h`'s
// `ForceInMemoryWorkspaceForTests` (in-memory `reldex-workspace` store,
// forced before any workspace is opened), so none of them touch the real
// on-disk store.
class TstServerOutputController : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void enabledAndBufferRoundTripThroughTheSettingsRegistryWithProvenance();
    void twoWorksheetsAreIsolatedSettingsAndOutputAlike();
    void invalidUtf8LinesAreCountedNotShownAsGarbage();
    void outputArrivesBeforeTheReplyThatFollowsIt();
    void truncationPastTheMocksCapIsReportedNeverSilent();
    void clearEmptiesTheModelAndTheTruncationState();
    void oneHundredThousandLinesAppendQuicklyWithoutPerLineObjects();
};

void TstServerOutputController::enabledAndBufferRoundTripThroughTheSettingsRegistryWithProvenance()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SettingsController settings;
    QVERIFY(settings.open());

    ServerOutputController pane(&bridge, &settings, newWorksheetId());
    QVERIFY(pane.open());
    QVERIFY(spinUntil([&pane] { return pane.ready(); }));

    // Off by default, and the value came from nowhere this worksheet or a
    // profile set (ADR-0006 P2: `effective = worksheet ?? profile ??
    // application ?? built-in`) -- nothing here ever wrote a row, so the
    // built-in default is what resolves.
    QVERIFY(!pane.enabled());
    QCOMPARE(pane.enabledLevel(), static_cast<int>(ServerOutputController::LevelBuiltIn));
    QVERIFY(!pane.unlimited());
    QCOMPARE(pane.bufferLevel(), static_cast<int>(ServerOutputController::LevelBuiltIn));

    QVERIFY(pane.setEnabled(true));
    QVERIFY(spinUntil([&pane] { return pane.enabled(); }));
    QCOMPARE(pane.enabledLevel(), static_cast<int>(ServerOutputController::LevelWorksheet));

    QVERIFY(pane.setBufferBytes(5000));
    QVERIFY(spinUntil([&pane] { return pane.bufferBytes() == 5000; }));
    QVERIFY(!pane.unlimited());
    QCOMPARE(pane.bufferLevel(), static_cast<int>(ServerOutputController::LevelWorksheet));

    QVERIFY(pane.setUnlimited());
    QVERIFY(spinUntil([&pane] { return pane.unlimited(); }));
    QCOMPARE(pane.bufferLevel(), static_cast<int>(ServerOutputController::LevelWorksheet));

    QVERIFY(pane.setEnabled(false));
    QVERIFY(spinUntil([&pane] { return !pane.enabled(); }));
    // Disabling still writes a worksheet-level override (`false`, not "no
    // longer set") -- provenance stays Worksheet, only the value changed.
    QCOMPARE(pane.enabledLevel(), static_cast<int>(ServerOutputController::LevelWorksheet));

    // clearEnabledOverride() removes the worksheet row -- back to inherited
    // (built-in, since no profile/application row exists either).
    QVERIFY(pane.clearEnabledOverride());
    QVERIFY(spinUntil(
            [&pane] { return pane.enabledLevel() == ServerOutputController::LevelBuiltIn; }));
    QVERIFY(!pane.enabled());
}

void TstServerOutputController::twoWorksheetsAreIsolatedSettingsAndOutputAlike()
{
    Bridge bridge;
    SettingsController settings;
    QVERIFY(settings.open());

    // One store (ADR-0006 P6), two worksheet-scoped panes -- the scenario the
    // registry's `worksheet ?? profile ?? application ?? built-in` resolution
    // exists for.
    ServerOutputController paneA(&bridge, &settings, newWorksheetId());
    ServerOutputController paneB(&bridge, &settings, newWorksheetId());
    QVERIFY(paneA.open());
    QVERIFY(paneB.open());
    QVERIFY(spinUntil([&paneA, &paneB] { return paneA.ready() && paneB.ready(); }));

    QVERIFY(!paneA.enabled());
    QVERIFY(!paneB.enabled());

    QVERIFY(paneA.setEnabled(true));
    QVERIFY(spinUntil([&paneA] { return paneA.enabled(); }));
    // B must not see A's write: two independent worksheet ids, one store.
    QVERIFY(!paneB.enabled());
    QCOMPARE(paneB.enabledLevel(), static_cast<int>(ServerOutputController::LevelBuiltIn));

    // Output is isolated too: each pane owns its own session, so only A's
    // sample statement produces lines in A's model.
    QVERIFY(paneA.runSampleStatement());
    QVERIFY(spinUntil([&paneA] { return paneA.model()->rowCount() > 0; }));
    QCOMPARE(paneB.model()->rowCount(), 0);

    // B still resolves off, unaffected by A's session ever running.
    QVERIFY(!paneB.enabled());
}

void TstServerOutputController::invalidUtf8LinesAreCountedNotShownAsGarbage()
{
    Bridge bridge;
    SettingsController settings;
    QVERIFY(settings.open());
    ServerOutputController pane(&bridge, &settings, newWorksheetId());
    QVERIFY(pane.open());
    QVERIFY(spinUntil([&pane] { return pane.ready(); }));
    QVERIFY(pane.setEnabled(true));
    QVERIFY(spinUntil([&pane] { return pane.enabled(); }));

    QVERIFY(pane.runSampleStatement());
    QVERIFY(spinUntil([&pane] { return pane.model()->rowCount() >= 3; }));

    // RELDEX_MOCK_STATEMENT_SERVER_OUTPUT's fixed three lines (reldex.h,
    // crates/ffi/tests/events.rs): the last one arrives as invalid UTF-8,
    // replaced with U+FFFD (M2.12) -- a count, and the replacement character
    // in the text, never raw bytes.
    QCOMPARE(pane.model()->rowCount(), 3);
    QCOMPARE(pane.model()->data(pane.model()->index(0, 0)).toString(),
             QStringLiteral("reldex: first line"));
    QCOMPARE(pane.model()->data(pane.model()->index(1, 0)).toString(), QString());
    const QString thirdLine = pane.model()->data(pane.model()->index(2, 0)).toString();
    QVERIFY2(thirdLine.contains(QChar(0xFFFD)),
             qPrintable(QStringLiteral("expected U+FFFD in %1").arg(thirdLine)));
    QCOMPARE(pane.invalidUtf8Lines(), 1u);
}

void TstServerOutputController::outputArrivesBeforeTheReplyThatFollowsIt()
{
    Bridge bridge;
    SettingsController settings;
    QVERIFY(settings.open());
    ServerOutputController pane(&bridge, &settings, newWorksheetId());
    QVERIFY(pane.open());
    QVERIFY(spinUntil([&pane] { return pane.ready(); }));
    QVERIFY(pane.setEnabled(true));
    QVERIFY(spinUntil([&pane] { return pane.enabled(); }));

    // ADR-0003 A32/M2.15 hand-off: SERVER_OUTPUT lies strictly between a
    // statement's EXECUTING and its EXECUTED. Proving the adapter preserves
    // that -- not just the FFI/db-core event order, which
    // `crates/ffi/tests/events.rs`'s
    // `server_output_arrives_ahead_of_the_reply_that_follows_it` already
    // covers -- means observing the model's row count from inside
    // `executed()` itself.
    int rowsWhenExecutedFired = -1;
    QObject::connect(pane.m_session, &SessionController::executed, [&] {
        rowsWhenExecutedFired = pane.model()->rowCount();
    });

    QVERIFY(pane.runSampleStatement());
    QVERIFY(spinUntil([&] { return rowsWhenExecutedFired != -1; }));
    QCOMPARE(rowsWhenExecutedFired, 3);
}

void TstServerOutputController::truncationPastTheMocksCapIsReportedNeverSilent()
{
    Bridge bridge;
    SettingsController settings;
    QVERIFY(settings.open());
    ServerOutputController pane(&bridge, &settings, newWorksheetId());
    QVERIFY(pane.open());
    QVERIFY(spinUntil([&pane] { return pane.ready(); }));
    QVERIFY(pane.setEnabled(true));
    QVERIFY(spinUntil([&pane] { return pane.enabled(); }));

    // Mirrors `crates/ffi/tests/events.rs`'s
    // `server_output_past_the_cap_is_dropped_and_counted_on_the_next_output`:
    // flood statements' worth of output without letting this thread's
    // event loop drain in between, so the session's per-session cap of 256
    // undrained `SERVER_OUTPUT` events (ADR-0003 A34) is exceeded. Exactly
    // 256 events' worth of lines (768) land, each reporting `dropped == 0`;
    // the other 44 statements' output never became an event at all -- per
    // ADR-0003 A34, the loss is reported "on the next delivered event or
    // TERMINAL", so it stays silent (correctly, not a bug) until this test
    // submits one more statement below. `runSampleStatement()` only submits
    // (one `reldex_session_execute` call) and returns.
    //
    // The Rust test's own barrier before it starts draining --
    // `reldex_hub_pending_events(hub) == expected`, waited on with
    // `wait_until` -- is a Rust-only harness introspection call, not part
    // of the C ABI, so this adapter test cannot ask "has the worker fully
    // finished producing the flood yet" the same way. What it *can* do,
    // since a plain sleep processes no events at all (unlike
    // `QCoreApplication::processEvents()`, which is what a queued wake
    // needs to be delivered in the first place), is give the worker a
    // generous, fixed head start with nothing able to drain during it --
    // the mock's per-statement work is trivial, so this is comfortably
    // enough for a real machine even under load, and if it is ever not,
    // this test times out loudly rather than silently reporting a wrong
    // split. Found necessary by running this test bare, repeatedly, outside
    // ctest's own capture (`tst_serveroutput.exe -o file,txt`): without
    // this wait, draining could start (and interleave with) the flood
    // still being produced, and the `rowCount() == 256 * 3` assertion below
    // was flaky (a partial count mid-settle, confirmed by temporary
    // diagnostic logging: it can sit at exactly 768 with 0 dropped
    // indefinitely once settled, which is correct -- the flakiness was
    // entirely in whether 768 had *already* been reached at this
    // checkpoint, never in what the final numbers were).
    constexpr int kFloodStatements = 300;
    for (int i = 0; i < kFloodStatements; ++i) {
        QVERIFY(pane.runSampleStatement());
    }
    QThread::msleep(500);

    QVERIFY(spinUntil([&pane] { return pane.model()->rowCount() >= 256 * 3; }, 30000));
    QCOMPARE(pane.model()->rowCount(), 256 * 3);
    QCOMPARE(pane.droppedLines(), 0u);

    // One more statement, submitted only now that the flood has settled,
    // carries the cumulative count of what was lost.
    QVERIFY(pane.runSampleStatement());
    QVERIFY(spinUntil([&pane] { return pane.droppedLines() > 0; }, 30000));
    QCOMPARE(pane.droppedLines(), static_cast<quint32>((kFloodStatements - 256) * 3));
    qInfo("M4.7: flooding %d statements without draining left %d lines shown and %u dropped",
          kFloodStatements, pane.model()->rowCount(), static_cast<unsigned>(pane.droppedLines()));
}

void TstServerOutputController::clearEmptiesTheModelAndTheTruncationState()
{
    Bridge bridge;
    SettingsController settings;
    QVERIFY(settings.open());
    ServerOutputController pane(&bridge, &settings, newWorksheetId());
    QVERIFY(pane.open());
    QVERIFY(spinUntil([&pane] { return pane.ready(); }));
    QVERIFY(pane.setEnabled(true));
    QVERIFY(spinUntil([&pane] { return pane.enabled(); }));

    QVERIFY(pane.runSampleStatement());
    QVERIFY(spinUntil([&pane] { return pane.model()->rowCount() > 0; }));
    QVERIFY(pane.invalidUtf8Lines() > 0);

    pane.clear();
    QCOMPARE(pane.model()->rowCount(), 0);
    QCOMPARE(pane.droppedLines(), 0u);
    QCOMPARE(pane.invalidUtf8Lines(), 0u);
    QVERIFY(!pane.readFailed());

    // The setting itself is untouched by Clear -- it is a local UI reset,
    // never a registry write (`ServerOutputModel::clear()`'s own doc
    // comment).
    QVERIFY(pane.enabled());
}

void TstServerOutputController::oneHundredThousandLinesAppendQuicklyWithoutPerLineObjects()
{
    // The QML-level proof that a ListView over this model never instantiates
    // one delegate per line lives in tst_coreinfo.cpp (the one binary in
    // this suite with a QML engine). This is the model's own half: appending
    // 100,000 lines is one beginInsertRows/endInsertRows pair over a
    // std::vector<QString>, not 100,000 QObjects, and its cost is measured
    // here rather than assumed (AGENTS.md "Performance").
    ServerOutputModel model;
    QStringList lines;
    lines.reserve(100000);
    for (int i = 0; i < 100000; ++i) {
        lines.append(QStringLiteral("DBMS_OUTPUT line %1 of 100000").arg(i));
    }

    QElapsedTimer timer;
    timer.start();
    model.appendLines(lines);
    const qint64 elapsedMs = timer.elapsed();
    qInfo("M4.7: ServerOutputModel::appendLines() took %lld ms for 100,000 lines",
          static_cast<long long>(elapsedMs));

    QCOMPARE(model.rowCount(), 100000);
    QCOMPARE(model.data(model.index(0, 0)).toString(), QStringLiteral("DBMS_OUTPUT line 0 of 100000"));
    QCOMPARE(model.data(model.index(99999, 0)).toString(),
             QStringLiteral("DBMS_OUTPUT line 99999 of 100000"));

    QElapsedTimer clearTimer;
    clearTimer.start();
    model.clear();
    qInfo("M4.7: ServerOutputModel::clear() took %lld ms for 100,000 lines",
          static_cast<long long>(clearTimer.elapsed()));
    QCOMPARE(model.rowCount(), 0);
}

QTEST_GUILESS_MAIN(TstServerOutputController)

#include "tst_serveroutput.moc"
