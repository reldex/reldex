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
    void formattedTextIsBoundedByTheCache();
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

void TstResultModel::formattedTextIsBoundedByTheCache()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ResultTableModel *model = bridge.session()->model();
    model->setMaxCachedBatches(4);

    QVERIFY(streamGeneratedQuery(bridge, 5000, 100, 2));
    QCOMPARE(model->batchCount(), 50);
    QCOMPARE(model->hydratedColumnCount(), 0);

    // Walk every batch, touching both formatted columns (ID and CREATED).
    for (int batch = 0; batch < 50; ++batch) {
        const int row = batch * 100 + 7;
        QVERIFY(!model->data(model->index(row, 0), Qt::DisplayRole).toString().isEmpty());
        QVERIFY(!model->data(model->index(row, 2), Qt::DisplayRole).toString().isEmpty());
        // Two formatted columns per cached batch, four batches cached.
        QVERIFY2(model->hydratedColumnCount() <= 8,
                 qPrintable(QStringLiteral("hydrated=%1 after batch %2")
                                    .arg(model->hydratedColumnCount())
                                    .arg(batch)));
    }
    QCOMPARE(model->hydratedColumnCount(), 8);

    // An evicted batch re-hydrates on demand and still reads correctly.
    QCOMPARE(model->data(model->index(0, 0), Qt::DisplayRole).toString(), QStringLiteral("1"));
    QVERIFY(model->hydratedColumnCount() <= 8);

    // Tightening the bound drops formatted text immediately.
    model->setMaxCachedBatches(1);
    QVERIFY(model->hydratedColumnCount() <= 2);

    // The text column never hydrates anything: it is read straight out of the
    // batch's own buffer (ADR-0003 D4's zero-copy path).
    const int hydratedBefore = model->hydratedColumnCount();
    for (int batch = 0; batch < 50; ++batch) {
        QVERIFY(model->data(model->index(batch * 100 + 3, 1), ResultTableModel::IsNullRole)
                        .isValid());
        QVERIFY(!model->data(model->index(batch * 100 + 3, 1), Qt::DisplayRole)
                         .toString()
                         .isEmpty());
    }
    QCOMPARE(model->hydratedColumnCount(), hydratedBefore);
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
    QElapsedTimer wall;
    wall.start();
    QVERIFY(streamGeneratedQuery(bridge, 1000000, 1000, 2, 0, 600000));
    const qint64 streamMs = wall.elapsed();
    const qint64 rssStreamed = Metrics::residentBytes();

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

    qInfo("1M sanity: streamed in %lld ms; RSS %lld -> %lld (delta %lld bytes, %.1f B/row); "
          "cold window %lld cells in %lld ns; warm %lld cells in %lld ns (%.1f ns/cell); "
          "RSS after reads %lld",
          static_cast<long long>(streamMs), static_cast<long long>(rssBefore),
          static_cast<long long>(rssStreamed),
          static_cast<long long>(rssStreamed - rssBefore),
          static_cast<double>(rssStreamed - rssBefore) / 1000000.0,
          static_cast<long long>(cells), static_cast<long long>(coldNs),
          static_cast<long long>(warmCells), static_cast<long long>(warmNs),
          warmCells > 0 ? static_cast<double>(warmNs) / static_cast<double>(warmCells) : 0.0,
          static_cast<long long>(rssAfter));
}

QTEST_GUILESS_MAIN(TstResultModel)

#include "tst_resultmodel.moc"
