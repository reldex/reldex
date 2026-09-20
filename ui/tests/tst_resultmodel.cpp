#include "AdapterTestSupport.h"

#include <Metrics.h>

#include <QAbstractItemModelTester>
#include <QElapsedTimer>
#include <QRandomGenerator>
#include <QTest>

#include <utility>

using adapter_test::spinUntil;
using adapter_test::streamGeneratedQuery;

// M1.6: `ResultTableModel` over borrowed batch views (ADR-0003 D4).
//
// Guiless on purpose -- none of this needs a window, and QAbstractItemModelTester
// is the test that matters. The QML side is covered by tst_coreinfo, which
// loads Main.qml (now a TableView over this model) offscreen.
class TstResultModel : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void modelTesterSurvivesAMultiBatchStream();
    void modelTesterSurvivesAResetMidStream();
    void cellsMatchWhatTheMockGenerated();
    void columnHeadersComeFromTheBatch();
    void fetchMoreDrivesTheStreamWhenAutoFetchIsOff();
    void aLargeBatchIsFormattedOneWindowAtATime();
    void formattedTextIsBoundedByTheCache();
    void theWindowBeingReadIsNeverEvicted();
    void rowsPastTheModelsCeilingAreRefused();
    void reExecutingDuringAStreamRunsTheNewResultToCompletion();
    void zeroRowsMeansTheMocksDocumentedDefault();
    void sanityStreamOfAMillionRows();
};

void TstResultModel::modelTesterSurvivesAMultiBatchStream()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();

    // Attached before anything streams, so every insertion is checked.
    QAbstractItemModelTester tester(model, QAbstractItemModelTester::FailureReportingMode::QtTest);

    QVERIFY(streamGeneratedQuery(bridge, 5000, 250, 3));
    QCOMPARE(bridge.session()->state(), SessionController::ResultComplete);
    QCOMPARE(model->rowCount(), 5000);
    QCOMPARE(model->columnCount(), 3);
    QCOMPARE(bridge.session()->rowsFetched(), qint64(5000));
    QVERIFY(model->batchCount() >= 20);
    QCOMPARE(bridge.orphanEvents(), qint64(0));
}

void TstResultModel::modelTesterSurvivesAResetMidStream()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    ResultTableModel *model = session->model();
    QAbstractItemModelTester tester(model, QAbstractItemModelTester::FailureReportingMode::QtTest);

    session->setMockRows(500000);
    session->setFetchRows(200);
    session->setMaxFetchesInFlight(4);
    session->setRunOnOpen(true);
    QVERIFY(session->open());

    QVERIFY(spinUntil([session] { return session->rowsFetched() >= 2000; }));
    QVERIFY(session->state() == SessionController::Fetching);

    // Close the result while fetches are still in flight. Every one of them
    // still gets exactly one reply; the model must end up empty and the late
    // batches must not be appended to it.
    QVERIFY(session->closeResult());
    QCOMPARE(model->rowCount(), 0);

    const qint64 settled = session->rowsFetched();
    QVERIFY(spinUntil([session] { return session->fetchesInFlight() == 0; }));
    QCOMPARE(model->rowCount(), 0);
    QCOMPARE(session->rowsFetched(), settled);

    // The session survived, so it can run another statement.
    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_DML));
    QVERIFY(spinUntil([session] {
        return session->state() == SessionController::Ready
                || session->state() == SessionController::Failed;
    }));
    QCOMPARE(session->state(), SessionController::Ready);
    QCOMPARE(session->rowsAffected(), qint64(1));
}

void TstResultModel::cellsMatchWhatTheMockGenerated()
{
    constexpr qint64 kRows = 3000;
    constexpr int kFetchRows = 256;
    constexpr qint64 kSeed = 7;

    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();
    // Deterministic date text; the default ISO-8601 style also carries a time
    // part, which is checked separately below.
    model->setTimestampStyle(RELDEX_TIMESTAMP_STYLE_DATE_ONLY);

    QVERIFY(streamGeneratedQuery(bridge, kRows, kFetchRows, 2, kSeed));
    QCOMPARE(model->rowCount(), static_cast<int>(kRows));

    QList<int> rows {
        0, // first row of the result and of batch 0
        kFetchRows - 1, // last row of a batch
        kFetchRows, // first row of the next batch
        9, // Thai (row number 10)
        24, // non-BMP emoji (row number 25)
        99, // NULL (row number 100)
        static_cast<int>(kRows) - 1, // last row of the result
    };
    QRandomGenerator generator(0x5EEDu);
    for (int index = 0; index < 64; ++index) {
        rows.append(static_cast<int>(generator.bounded(static_cast<quint32>(kRows))));
    }

    for (const int row : std::as_const(rows)) {
        const auto rowNumber = static_cast<quint64>(row) + 1;

        const QVariant id = model->data(model->index(row, 0), Qt::DisplayRole);
        QCOMPARE(id.toString(), adapter_test::expectedId(rowNumber));
        QCOMPARE(model->data(model->index(row, 0), ResultTableModel::IsNullRole).toBool(), false);

        const QModelIndex nameIndex = model->index(row, 1);
        const bool isNull = model->data(nameIndex, ResultTableModel::IsNullRole).toBool();
        QCOMPARE(isNull, adapter_test::expectedNameIsNull(rowNumber));
        if (isNull) {
            QCOMPARE(model->data(nameIndex, Qt::DisplayRole).toString(), model->nullText());
        } else {
            QCOMPARE(model->data(nameIndex, Qt::DisplayRole).toString(),
                     adapter_test::expectedName(rowNumber, static_cast<quint64>(kSeed)));
        }

        QCOMPARE(model->data(model->index(row, 2), Qt::DisplayRole).toString(),
                 adapter_test::expectedCreatedDateOnly(rowNumber));
    }

    // Row 25 really is non-BMP, and row 10 really is Thai -- the thing spike
    // S11 said the adapter must not get wrong.
    const QString emojiRow = model->data(model->index(24, 1), Qt::DisplayRole).toString();
    bool sawSurrogatePair = false;
    for (qsizetype index = 0; index + 1 < emojiRow.size(); ++index) {
        if (emojiRow.at(index).isHighSurrogate() && emojiRow.at(index + 1).isLowSurrogate()) {
            sawSurrogatePair = true;
            break;
        }
    }
    QVERIFY2(sawSurrogatePair, qPrintable(emojiRow));

    const QString thaiRow = model->data(model->index(9, 1), Qt::DisplayRole).toString();
    QVERIFY2(thaiRow.contains(QChar(0x0E02)) || thaiRow.contains(QChar(0x0E17))
                     || thaiRow.contains(QChar(0x0E41)) || thaiRow.contains(QChar(0x0E10)),
             qPrintable(thaiRow));

    // The default ISO-8601 style keeps the same date and adds the time part a
    // DATE carries.
    model->setTimestampStyle(RELDEX_TIMESTAMP_STYLE_ISO8601);
    const QString iso = model->data(model->index(0, 2), Qt::DisplayRole).toString();
    QVERIFY2(iso.startsWith(adapter_test::expectedCreatedDateOnly(1)), qPrintable(iso));

    // An out-of-range index is an empty QVariant, never a read past the end.
    QVERIFY(!model->data(model->index(0, 0), Qt::DecorationRole).isValid());
    QVERIFY(!model->data(QModelIndex(), Qt::DisplayRole).isValid());
}

void TstResultModel::columnHeadersComeFromTheBatch()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();
    QVERIFY(streamGeneratedQuery(bridge, 100, 50, 2));

    QCOMPARE(model->headerData(0, Qt::Horizontal, Qt::DisplayRole).toString(),
             QStringLiteral("ID"));
    QCOMPARE(model->headerData(1, Qt::Horizontal, Qt::DisplayRole).toString(),
             QStringLiteral("NAME"));
    QCOMPARE(model->headerData(2, Qt::Horizontal, Qt::DisplayRole).toString(),
             QStringLiteral("CREATED"));
    QCOMPARE(model->headerData(0, Qt::Vertical, Qt::DisplayRole).toInt(), 1);
    QVERIFY(!model->headerData(3, Qt::Horizontal, Qt::DisplayRole).isValid());
}

void TstResultModel::fetchMoreDrivesTheStreamWhenAutoFetchIsOff()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    ResultTableModel *model = session->model();

    session->setMockRows(1000);
    session->setFetchRows(100);
    session->setMaxFetchesInFlight(1);
    session->setAutoFetch(false);
    session->setRunOnOpen(true);
    QVERIFY(session->open());

    QVERIFY(spinUntil([session] { return session->state() == SessionController::Fetching; }));
    QCOMPARE(model->rowCount(), 0);
    QVERIFY(model->canFetchMore(QModelIndex()));

    for (int step = 1; step <= 3; ++step) {
        model->fetchMore(QModelIndex());
        const int expected = step * 100;
        QVERIFY(spinUntil([model, expected] { return model->rowCount() >= expected; }));
        QCOMPARE(model->rowCount(), expected);
    }

    // Nothing arrives on its own while auto-fetch is off.
    QVERIFY(!spinUntil([model] { return model->rowCount() > 300; }, 200));
    QCOMPARE(model->rowCount(), 300);
}

void TstResultModel::aLargeBatchIsFormattedOneWindowAtATime()
{
    // The reason `hydrate()` is keyed on a window and not on a batch: a batch is
    // whatever `fetchRows` says, so formatting a whole one puts an unbounded
    // cost inside the first cell read from it.
    constexpr int kRows = 50000;
    constexpr qint64 kSeed = 3;

    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();
    model->setTimestampStyle(RELDEX_TIMESTAMP_STYLE_DATE_ONLY);

    QVERIFY(streamGeneratedQuery(bridge, kRows, kRows, 1, kSeed));
    QCOMPARE(model->rowCount(), kRows);
    QCOMPARE(model->batchCount(), 1); // one batch, 49 windows in it
    QCOMPARE(model->formattedWindowCount(), 0);

    const int windowRows = ResultTableModel::formatWindowRows();
    QVERIFY(windowRows > 0);

    // One cell read renders exactly one window of one column.
    QCOMPARE(model->data(model->index(0, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(1));
    QCOMPARE(model->formattedWindowCount(), 1);
    const qint64 oneWindowBytes = model->formattedBytes();
    QVERIFY(oneWindowBytes > 0);
    // Far below what rendering the whole 50,000-row batch would cost. The bound
    // is what is being asserted, not a timing.
    QVERIFY2(oneWindowBytes < 100 * 1024,
             qPrintable(QStringLiteral("one window cost %1 bytes").arg(oneWindowBytes)));

    // A second cell in the same window renders nothing further.
    QCOMPARE(model->data(model->index(windowRows - 1, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(static_cast<quint64>(windowRows)));
    QCOMPARE(model->formattedWindowCount(), 1);
    QCOMPARE(model->formattedBytes(), oneWindowBytes);

    // The same for the DATE column, which is the widest formatted one in the
    // S14 shape. These two numbers are what ui/README.md's memory bound is
    // derived from, kept where they are produced rather than only in prose.
    QVERIFY(!model->data(model->index(0, 2), Qt::DisplayRole).toString().isEmpty());
    QCOMPARE(model->formattedWindowCount(), 2);
    qInfo("a %d-row window costs %lld bytes (NUMBER) and %lld bytes (DATE); the default bound is "
          "%d windows / %lld bytes",
          windowRows, static_cast<long long>(oneWindowBytes),
          static_cast<long long>(model->formattedBytes() - oneWindowBytes),
          model->maxFormattedWindows(), static_cast<long long>(model->maxFormattedBytes()));

    // Cells in *different* windows of the same batch are each correct -- the
    // failure this guards against is a window's offsets being read with the
    // wrong base row, which produces neighbouring-but-wrong text.
    const QList<int> rows { 0,
                            windowRows - 1,
                            windowRows,
                            windowRows + 1,
                            3 * windowRows + 17,
                            kRows / 2,
                            kRows - windowRows,
                            kRows - 1 };
    for (const int row : rows) {
        const auto rowNumber = static_cast<quint64>(row) + 1;
        QCOMPARE(model->data(model->index(row, 0), Qt::DisplayRole).toString(),
                 adapter_test::expectedId(rowNumber));
        QCOMPARE(model->data(model->index(row, 2), Qt::DisplayRole).toString(),
                 adapter_test::expectedCreatedDateOnly(rowNumber));
    }

    // The text column still goes down the zero-copy path: no window for it.
    const int windowsAfterNumbers = model->formattedWindowCount();
    for (const int row : rows) {
        QVERIFY(model->data(model->index(row, 1), ResultTableModel::IsNullRole).isValid());
        QVERIFY(!model->data(model->index(row, 1), Qt::DisplayRole).toString().isEmpty());
    }
    QCOMPARE(model->formattedWindowCount(), windowsAfterNumbers);
}

void TstResultModel::formattedTextIsBoundedByTheCache()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();
    model->setMaxFormattedWindows(4);

    QVERIFY(streamGeneratedQuery(bridge, 5000, 100, 2));
    QCOMPARE(model->batchCount(), 50);
    QCOMPARE(model->formattedWindowCount(), 0);
    QCOMPARE(model->formattedBytes(), qint64(0));

    // Walk every batch, touching both formatted columns (ID and CREATED). Each
    // batch is 100 rows, so each (batch, column) is exactly one window.
    for (int batch = 0; batch < 50; ++batch) {
        const int row = batch * 100 + 7;
        QVERIFY(!model->data(model->index(row, 0), Qt::DisplayRole).toString().isEmpty());
        QVERIFY(!model->data(model->index(row, 2), Qt::DisplayRole).toString().isEmpty());
        QVERIFY2(model->formattedWindowCount() <= 4,
                 qPrintable(QStringLiteral("cached=%1 after batch %2")
                                    .arg(model->formattedWindowCount())
                                    .arg(batch)));
    }
    QCOMPARE(model->formattedWindowCount(), 4);

    // An evicted window re-renders on demand and still reads correctly.
    QCOMPARE(model->data(model->index(0, 0), Qt::DisplayRole).toString(), QStringLiteral("1"));
    QCOMPARE(model->data(model->index(4999, 0), Qt::DisplayRole).toString(),
             QStringLiteral("5000"));
    QVERIFY(model->formattedWindowCount() <= 4);

    // Tightening either bound evicts immediately.
    model->setMaxFormattedWindows(1);
    QCOMPARE(model->formattedWindowCount(), 1);
    model->setMaxFormattedBytes(0);
    QCOMPARE(model->formattedWindowCount(), 0);
    QCOMPARE(model->formattedBytes(), qint64(0));

    // A zero byte-bound does not break reading; it only means every read
    // renders. (The cache is a bound, not a correctness requirement.)
    QCOMPARE(model->data(model->index(2500, 0), Qt::DisplayRole).toString(),
             QStringLiteral("2501"));

    // The text column never renders anything: it is read straight out of the
    // batch's own buffer (ADR-0003 D4's zero-copy path).
    model->setMaxFormattedBytes(8LL * 1024 * 1024);
    model->setMaxFormattedWindows(64);
    const int cachedBefore = model->formattedWindowCount();
    for (int batch = 0; batch < 50; ++batch) {
        QVERIFY(model->data(model->index(batch * 100 + 3, 1), ResultTableModel::IsNullRole)
                        .isValid());
        QVERIFY(!model->data(model->index(batch * 100 + 3, 1), Qt::DisplayRole)
                         .toString()
                         .isEmpty());
    }
    QCOMPARE(model->formattedWindowCount(), cachedBefore);
    QVERIFY(!model->formattingFailed());
}

void TstResultModel::theWindowBeingReadIsNeverEvicted()
{
    // The bound is allowed to be exceeded by exactly one window: the one being
    // read. Evicting it would make the very next cell re-render it, which is
    // the per-cell FFI call D4 exists to avoid.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();

    QVERIFY(streamGeneratedQuery(bridge, 4096, 4096, 1));
    QCOMPARE(model->batchCount(), 1);

    model->setMaxFormattedWindows(1);
    model->setMaxFormattedBytes(1); // smaller than any real window

    for (int window = 0; window < 4; ++window) {
        const int row = window * ResultTableModel::formatWindowRows() + 5;
        QCOMPARE(model->data(model->index(row, 0), Qt::DisplayRole).toString(),
                 adapter_test::expectedId(static_cast<quint64>(row) + 1));
        // Exactly one survives: the one just read, kept so the next cell in it
        // is free.
        QCOMPARE(model->formattedWindowCount(), 1);
    }

    // Reading the same window twice in a row must still hit the cache, which is
    // the whole point of keeping it.
    const qint64 bytes = model->formattedBytes();
    QCOMPARE(model->data(model->index(6, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(7));
    QCOMPARE(model->formattedWindowCount(), 1);
    QVERIFY(model->formattedBytes() > 0);
    QVERIFY(bytes > 0);
}

void TstResultModel::rowsPastTheModelsCeilingAreRefused()
{
    // `QAbstractItemModel` counts rows in `int`. A stream longer than that must
    // stop rather than wrap, and must say so. `maxRows` makes that path
    // testable without 2^31 rows.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    ResultTableModel *model = session->model();
    QAbstractItemModelTester tester(model, QAbstractItemModelTester::FailureReportingMode::QtTest);

    QVERIFY(!model->rowLimitReached());
    model->setMaxRows(250);
    // Said once, and said exactly like this: 100 + 100 + 100 rows against a
    // ceiling of 250 refuses the last 50 of the third batch and every batch
    // after it. `ignoreMessage()` consumes one occurrence, so a version that
    // warned per batch would still show up in the log.
    QTest::ignoreMessage(QtWarningMsg,
                         "ResultTableModel: row limit 250 reached; 50 further row(s) in this "
                         "batch and every later batch are refused. What was already fetched "
                         "stays readable.");

    session->setMockRows(10000);
    session->setFetchRows(100);
    session->setMaxFetchesInFlight(1);
    session->setRunOnOpen(true);
    QVERIFY(session->open());

    QVERIFY(spinUntil([model] { return model->rowLimitReached(); }));

    // The refusal is announced once and fetching stops; what was fetched stays
    // readable, and the partial batch is truncated rather than dropped.
    QVERIFY(spinUntil([session] { return session->fetchesInFlight() == 0; }));
    QCOMPARE(model->rowCount(), 250);
    QVERIFY(!model->canFetchMore(QModelIndex()));
    QVERIFY(!session->canFetchMoreRows());
    QCOMPARE(model->data(model->index(249, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(250));

    // Nothing arrives afterwards, even though the result has 9,750 rows left.
    QVERIFY(!spinUntil([model] { return model->rowCount() > 250; }, 200));
    QCOMPARE(model->rowCount(), 250);

    // A new result clears the flag.
    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_DML));
    QVERIFY(spinUntil([session] {
        return session->state() == SessionController::Ready
                || session->state() == SessionController::Failed;
    }));
    QCOMPARE(session->state(), SessionController::Ready);
    QVERIFY(!model->rowLimitReached());
}

void TstResultModel::reExecutingDuringAStreamRunsTheNewResultToCompletion()
{
    // The stale-reply path: fetches issued against the replaced result still
    // get exactly one reply each (A5), and the in-flight slot each one frees
    // has to be handed to the current result.
    //
    // Per-session FIFO (A5) happens to deliver those replies *before* the new
    // EXECUTED, so the new result finds the slots already free -- but the
    // adapter must not depend on that ordering, and this test passes either
    // way only because the stale path re-submits too.
    // No QAbstractItemModelTester here on purpose: it re-runs its whole suite
    // on every rowsInserted, and that suite walks the model's rows, so
    // attaching it to a long stream makes the *test* quadratic rather than the
    // model. Model consistency is covered by the tests above, which stream a
    // few thousand rows; this one is about the session's bookkeeping.
    constexpr int kRows = 20000;

    Bridge bridge;
    QVERIFY(bridge.isValid());
    SessionController *session = bridge.session();
    ResultTableModel *model = session->model();

    session->setMockRows(kRows);
    session->setFetchRows(200);
    session->setMaxFetchesInFlight(4);
    session->setRunOnOpen(true);
    QVERIFY(session->open());
    QVERIFY(spinUntil([session] { return session->rowsFetched() >= 1000; }));
    QVERIFY(session->fetchesInFlight() > 0);

    // Re-execute with every slot busy. The row count is the scenario's, fixed
    // when the session was opened, so the new result has `kRows` rows too.
    QVERIFY(session->executeMockStatement(RELDEX_MOCK_STATEMENT_GENERATED_QUERY));
    QVERIFY(spinUntil([session] {
        return session->state() == SessionController::ResultComplete
                || session->state() == SessionController::Failed;
    }));
    QCOMPARE(session->state(), SessionController::ResultComplete);
    QCOMPARE(model->rowCount(), kRows);
    QCOMPARE(session->rowsFetched(), qint64(kRows));
    QCOMPARE(session->fetchesInFlight(), 0);
    QCOMPARE(bridge.orphanEvents(), qint64(0));

    // The rows are the new result's, from its first row to its last -- nothing
    // of the replaced result's stream was appended to it.
    QCOMPARE(model->data(model->index(0, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(1));
    QCOMPARE(model->data(model->index(kRows - 1, 0), Qt::DisplayRole).toString(),
             adapter_test::expectedId(kRows));
}

void TstResultModel::zeroRowsMeansTheMocksDocumentedDefault()
{
    // reldex.h, ReldexMockScenarioConfig::rows: "0 means 1,000". Checked here
    // because every other test in this file passes an explicit row count and
    // would not notice if the default moved.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();

    QVERIFY(streamGeneratedQuery(bridge, 0, 100, 1));
    QCOMPARE(model->rowCount(), 1000);
    QCOMPARE(model->columnCount(), 3);
    QCOMPARE(model->headerData(0, Qt::Horizontal, Qt::DisplayRole).toString(),
             QStringLiteral("ID"));
}

void TstResultModel::sanityStreamOfAMillionRows()
{
    // Information only, and NOT part of the normal run: spike S15's numbers
    // are M1.8's to produce, on the method AGENTS.md "Performance" requires.
    // This exists so M1.6 can say whether the shape is plausible at all, and
    // so M1.8 has a scripted starting point:
    //
    //   RELDEX_UI_SANITY_1M=1 ./tst_resultmodel sanityStreamOfAMillionRows
    if (adapter_test::envNumber("RELDEX_UI_SANITY_1M", 0) == 0) {
        QSKIP("set RELDEX_UI_SANITY_1M=1 to run the 1,000,000-row sanity stream");
    }

    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();

    const qint64 rssBefore = Metrics::residentBytes();
    const qint64 privateBefore = Metrics::privateBytes();
    QElapsedTimer wall;
    wall.start();
    QVERIFY(streamGeneratedQuery(bridge, 1000000, 1000, 2, 0, 600000));
    const qint64 streamMs = wall.elapsed();
    const qint64 rssStreamed = Metrics::residentBytes();
    const qint64 privateStreamed = Metrics::privateBytes();

    QCOMPARE(model->rowCount(), 1000000);

    // Read a window the way a viewport would, so the formatted-text cache is
    // warm for the cells that are on screen.
    QElapsedTimer window;
    window.start();
    qint64 cells = 0;
    for (int row = 500000; row < 500040; ++row) {
        for (int column = 0; column < 3; ++column) {
            QVERIFY(model->data(model->index(row, column), Qt::DisplayRole).isValid());
            ++cells;
        }
    }
    const qint64 coldNs = window.nsecsElapsed();

    // The same window again, with the formatted text already rendered: this is
    // the per-cell cost a repaint pays, which is what K4's 200 ns/cell budget
    // is about.
    window.restart();
    qint64 warmCells = 0;
    for (int repeat = 0; repeat < 100; ++repeat) {
        for (int row = 500000; row < 500040; ++row) {
            for (int column = 0; column < 3; ++column) {
                QVERIFY(model->data(model->index(row, column), Qt::DisplayRole).isValid());
                ++warmCells;
            }
        }
    }
    const qint64 warmNs = window.nsecsElapsed();
    const qint64 rssAfter = Metrics::residentBytes();

    qInfo("1M sanity: streamed in %lld ms; RSS delta %lld bytes (%.1f B/row); private delta %lld "
          "bytes (%.1f B/row); cold window %lld cells in %lld ns; warm %lld cells in %lld ns "
          "(%.1f ns/cell); RSS after reads %lld; formatted cache %d windows / %lld bytes",
          static_cast<long long>(streamMs),
          static_cast<long long>(rssStreamed - rssBefore),
          static_cast<double>(rssStreamed - rssBefore) / 1000000.0,
          static_cast<long long>(privateStreamed - privateBefore),
          static_cast<double>(privateStreamed - privateBefore) / 1000000.0,
          static_cast<long long>(cells), static_cast<long long>(coldNs),
          static_cast<long long>(warmCells), static_cast<long long>(warmNs),
          warmCells > 0 ? static_cast<double>(warmNs) / static_cast<double>(warmCells) : 0.0,
          static_cast<long long>(rssAfter), model->formattedWindowCount(),
          static_cast<long long>(model->formattedBytes()));
}

QTEST_GUILESS_MAIN(TstResultModel)

#include "tst_resultmodel.moc"
