#include "AdapterTestSupport.h"

#include <ObjectBrowserModel.h>
#include <ReldexHandles.h>

#include <QAbstractItemModelTester>
#include <QByteArray>
#include <QSignalSpy>
#include <QTest>

#include <array>
#include <memory>

using adapter_test::spinUntil;

// M6.1: `ObjectBrowserModel`'s tree, session-ownership, truncation and
// error-mapping behavior.
//
// What this suite can and cannot prove today (see `ObjectBrowserModel.h`'s
// top-of-file doc comment for the full writeup and file/line references):
//
//  * Own metadata session, distinct from a worksheet's, opened lazily and
//    closed on demand: real, end-to-end, against the real FFI
//    (`ownMetadataSessionIsCountedSeparatelyFromAWorksheetSession`).
//  * `reldex_metadata_prepare`/`reldex_metadata_query_reclassify_error`: also
//    real FFI calls, exercised directly
//    (`theRealClassifierTurnsAnAmbiguousOracleCodeIntoPermission`).
//  * A fetch actually returning rows and a fetch failing with a permission
//    code are NOT exercisable end-to-end: `reldex_session_execute()` cannot
//    carry the binds every metadata statement requires, and the only mock
//    scenario this build's `reldex_hub_open_session()` can open (`S14`) has
//    no metadata fixtures wired into it at the FFI layer. What IS real and
//    tested end-to-end is that this fails *safely* -- a typed, generic
//    error, never a crash, never invented rows
//    (`expandingFailsSafelyAgainstTheOnlyMockSceneThisBuildCanOpen`).
//  * The tree-building, truncation and error-mapping logic itself
//    (`applyRows`/`populateStaticGroups`/`describeError`) is exercised
//    directly, as the friended `tst_ObjectBrowserModel`, since it is
//    otherwise unreachable without a fetch that can succeed.
class tst_ObjectBrowserModel : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void theRootHasOneConnectionNodeOnceABridgeIsSet();
    void ownMetadataSessionIsCountedSeparatelyFromAWorksheetSession();
    void expandingFailsSafelyAgainstTheOnlyMockSceneThisBuildCanOpen();
    void populateStaticGroupsBuildsTheNineSpecGroupsInOrder();
    void applyRowsBuildsObjectChildrenFromObjectsOfKindShapedRows();
    void applyRowsBuildsColumnChildrenFromColumnsOfShapedRows();
    void applyRowsSetsTheTruncationIndicatorAndCount();
    void setFilterIsANoOpForColumnsAndLeafNodes();
    void describeErrorNeverReturnsTheSameTextForPermissionAndOther();
    void theRealClassifierTurnsAnAmbiguousOracleCodeIntoPermission();
    void applyingFiveThousandObjectsDoesNotBlockTheUiThreadForLong();
    void aFilterChangeWhileTheFirstFetchIsInFlightSupersedesRatherThanBeingDropped();
    void destroyingTheModelWhileAFetchIsInFlightDoesNotCrash();
    void destroyingTheBridgeBeforeTheModelDoesNotCrash();

private:
    // Builds a bare `ObjectBrowserModel::Node` the test owns directly
    // (never attached to a model's tree), for exercising a private helper in
    // isolation. `ObjectBrowserModel::Node` is a private nested type, visible
    // here only because of the `friend` declaration.
    static std::unique_ptr<ObjectBrowserModel::Node> makeNode(ObjectBrowserModel::NodeKind kind);
};

std::unique_ptr<ObjectBrowserModel::Node> tst_ObjectBrowserModel::makeNode(
        ObjectBrowserModel::NodeKind kind)
{
    auto node = std::make_unique<ObjectBrowserModel::Node>();
    node->kind = kind;
    return node;
}

void tst_ObjectBrowserModel::theRootHasOneConnectionNodeOnceABridgeIsSet()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());

    ObjectBrowserModel model;
    QCOMPARE(model.rowCount(), 0);

    model.setBridge(&bridge);
    QCOMPARE(model.rowCount(), 1);
    const QModelIndex connection = model.index(0, 0);
    QVERIFY(connection.isValid());
    QCOMPARE(model.data(connection, ObjectBrowserModel::KindRole).toInt(),
             static_cast<int>(ObjectBrowserModel::NodeKind::Connection));
    // Expandable immediately, before anything has been fetched -- that is
    // what makes lazy loading possible.
    QVERIFY(model.hasChildren(connection));
    QCOMPARE(model.rowCount(connection), 0);
}

void tst_ObjectBrowserModel::ownMetadataSessionIsCountedSeparatelyFromAWorksheetSession()
{
    // Since M2.15 (ADR-0003 A33) a session stops counting in
    // `reldex_hub_session_count()` the moment its `TERMINAL` is drained. M6.1
    // found it never did before (the interim pump never removed the entry;
    // see `ui/README.md`), so this asserts the count both ways: up by one
    // per session opened, down by one per session closed.
    Bridge bridge;
    QVERIFY(bridge.isValid());
    const qint64 baseline = static_cast<qint64>(reldex_hub_session_count(bridge.hub()));

    // A worksheet's own session, exactly like a worksheet would open one.
    SessionController *worksheet = bridge.session();
    QVERIFY(worksheet->open());
    QVERIFY(spinUntil([worksheet] { return worksheet->sessionId() != 0; }));
    QCOMPARE(static_cast<qint64>(reldex_hub_session_count(bridge.hub())), baseline + 1);

    // The object browser's own session -- ADR-0002: never the worksheet's.
    ObjectBrowserModel model;
    model.setBridge(&bridge);
    QVERIFY(!model.isSessionOpen());
    model.expand(model.index(0, 0)); // Connection: opens the metadata session lazily
    QVERIFY(spinUntil([&model] { return model.sessionId() != 0; }));
    QVERIFY(model.sessionId() != worksheet->sessionId());
    QCOMPARE(static_cast<qint64>(reldex_hub_session_count(bridge.hub())), baseline + 2);

    // Collapsing/disconnecting the browser closes only its own session:
    // through the SessionController's own state machine, its
    // `sessionClosed` signal, and the hub's count.
    QSignalSpy closedSpy(model.m_metadataSession, &SessionController::sessionClosed);
    model.closeConnection();
    QVERIFY(spinUntil([&closedSpy] { return closedSpy.count() == 1; }));
    QCOMPARE(closedSpy.constFirst().at(0).toInt(),
             static_cast<int>(RELDEX_CLOSE_OUTCOME_CLOSED));
    QCOMPARE(closedSpy.constFirst().at(1).toBool(), false); // not left open
    QVERIFY(spinUntil([&model] { return model.m_metadataSession == nullptr; }));
    QVERIFY(!model.isSessionOpen());
    QVERIFY(spinUntil([&bridge, baseline] {
        return static_cast<qint64>(reldex_hub_session_count(bridge.hub())) == baseline + 1;
    }));

    // The worksheet's session is untouched by any of the above.
    QCOMPARE(worksheet->sessionId() != 0, true);
    QCOMPARE(worksheet->state(), SessionController::Ready);
    QVERIFY(worksheet->closeSession());
    QVERIFY(spinUntil([worksheet] { return worksheet->state() == SessionController::Closed; }));
    QVERIFY(spinUntil([&bridge, baseline] {
        return static_cast<qint64>(reldex_hub_session_count(bridge.hub())) == baseline;
    }));
}

void tst_ObjectBrowserModel::expandingFailsSafelyAgainstTheOnlyMockSceneThisBuildCanOpen()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());

    ObjectBrowserModel model;
    model.setBridge(&bridge);
    const QModelIndex connection = model.index(0, 0);

    model.expand(connection);
    QVERIFY(spinUntil(
            [&model, &connection] {
                return model.data(connection, ObjectBrowserModel::HasErrorRole).toBool();
            },
            30000));

    // Never a crash, never invented rows -- a typed, generic message. The
    // mock's actual failure is `ErrorKind::Other` ("no scripted response for
    // statement ..."), which carries no native code and is not one of the
    // codes the metadata classifier corrects, so it stays generic rather than
    // (wrongly) reading as a permission problem.
    QCOMPARE(model.rowCount(connection), 0);
    const QString errorText =
            model.data(connection, ObjectBrowserModel::ErrorTextRole).toString();
    QVERIFY(!errorText.isEmpty());
    QCOMPARE(errorText, ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_OTHER));
    QVERIFY(errorText != ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_PERMISSION));

    // A later expand() (e.g. the user retries) does not get stuck: the
    // failure cleared `childrenLoaded`, so this queues a fresh attempt
    // instead of silently doing nothing.
    model.expand(connection);
    QVERIFY(spinUntil([&model, &connection] {
        return model.data(connection, ObjectBrowserModel::HasErrorRole).toBool();
    }));
}

void tst_ObjectBrowserModel::populateStaticGroupsBuildsTheNineSpecGroupsInOrder()
{
    ObjectBrowserModel model;
    auto schema = makeNode(ObjectBrowserModel::NodeKind::Schema);
    schema->parent = model.m_root.get();
    schema->name = QStringLiteral("HR");
    schema->schema = QStringLiteral("HR");
    ObjectBrowserModel::Node *schemaPtr = schema.get();
    model.m_root->children.push_back(std::move(schema));

    model.populateStaticGroups(schemaPtr);

    QCOMPARE(static_cast<int>(schemaPtr->children.size()), 9);
    const QStringList expectedNames = {
        QStringLiteral("Tables"),   QStringLiteral("Views"),          QStringLiteral("Packages"),
        QStringLiteral("Package Bodies"), QStringLiteral("Procedures"), QStringLiteral("Functions"),
        QStringLiteral("Triggers"), QStringLiteral("Sequences"),      QStringLiteral("Synonyms"),
    };
    const std::array<int, 9> expectedKinds = {
        RELDEX_METADATA_OBJECT_KIND_TABLES,         RELDEX_METADATA_OBJECT_KIND_VIEWS,
        RELDEX_METADATA_OBJECT_KIND_PACKAGES,       RELDEX_METADATA_OBJECT_KIND_PACKAGE_BODIES,
        RELDEX_METADATA_OBJECT_KIND_PROCEDURES,     RELDEX_METADATA_OBJECT_KIND_FUNCTIONS,
        RELDEX_METADATA_OBJECT_KIND_TRIGGERS,       RELDEX_METADATA_OBJECT_KIND_SEQUENCES,
        RELDEX_METADATA_OBJECT_KIND_SYNONYMS,
    };
    for (int i = 0; i < 9; ++i) {
        const auto &child = schemaPtr->children[static_cast<std::size_t>(i)];
        QCOMPARE(child->kind, ObjectBrowserModel::NodeKind::Group);
        QCOMPARE(child->name, expectedNames[i]);
        QCOMPARE(child->objectKind, expectedKinds[static_cast<std::size_t>(i)]);
        QCOMPARE(child->schema, QStringLiteral("HR"));
    }

    // Idempotent: groups never change, so a second call is a no-op rather
    // than duplicating rows.
    model.populateStaticGroups(schemaPtr);
    QCOMPARE(static_cast<int>(schemaPtr->children.size()), 9);
}

void tst_ObjectBrowserModel::applyRowsBuildsObjectChildrenFromObjectsOfKindShapedRows()
{
    ObjectBrowserModel model;
    auto group = makeNode(ObjectBrowserModel::NodeKind::Group);
    group->parent = model.m_root.get();
    group->schema = QStringLiteral("HR");
    group->objectKind = RELDEX_METADATA_OBJECT_KIND_TABLES;
    ObjectBrowserModel::Node *groupPtr = group.get();
    model.m_root->children.push_back(std::move(group));

    // objects_of_kind_columns(): name, status, created, last_modified.
    const QVector<QStringList> rows = {
        { QStringLiteral("EMPLOYEES"), QStringLiteral("VALID"), QStringLiteral("2019-01-01"),
          QStringLiteral("2022-01-01") },
        { QStringLiteral("DEPARTMENTS"), QStringLiteral("INVALID"), QStringLiteral("2019-01-01"),
          QStringLiteral("2019-01-01") },
    };
    model.applyRows(groupPtr, rows, /* truncated = */ false);

    QCOMPARE(static_cast<int>(groupPtr->children.size()), 2);
    const auto &employees = groupPtr->children[0];
    QCOMPARE(employees->kind, ObjectBrowserModel::NodeKind::Object);
    QCOMPARE(employees->name, QStringLiteral("EMPLOYEES"));
    QCOMPARE(employees->schema, QStringLiteral("HR"));
    QVERIFY(employees->isTableLike); // Tables get a Columns child
    QVERIFY(employees->secondary.isEmpty()); // VALID is not surfaced

    const auto &departments = groupPtr->children[1];
    QCOMPARE(departments->secondary, QStringLiteral("INVALID")); // a non-VALID status IS surfaced
    QVERIFY(!groupPtr->loading);
    QVERIFY(!groupPtr->hasError);
    QCOMPARE(groupPtr->shownCount, 2);
}

void tst_ObjectBrowserModel::applyRowsBuildsColumnChildrenFromColumnsOfShapedRows()
{
    ObjectBrowserModel model;
    auto columnsNode = makeNode(ObjectBrowserModel::NodeKind::ColumnsNode);
    columnsNode->parent = model.m_root.get();
    ObjectBrowserModel::Node *columnsPtr = columnsNode.get();
    model.m_root->children.push_back(std::move(columnsNode));

    // columns_of_columns(): position, name, type_name, nullable.
    const QVector<QStringList> rows = {
        { QStringLiteral("1"), QStringLiteral("EMPLOYEE_ID"), QStringLiteral("NUMBER"),
          QStringLiteral("NO") },
        { QStringLiteral("2"), QStringLiteral("COMMISSION_PCT"), QStringLiteral("NUMBER"),
          QStringLiteral("YES") },
    };
    model.applyRows(columnsPtr, rows, /* truncated = */ false);

    QCOMPARE(static_cast<int>(columnsPtr->children.size()), 2);
    const auto &commission = columnsPtr->children[1];
    QCOMPARE(commission->kind, ObjectBrowserModel::NodeKind::Column);
    QCOMPARE(commission->columnPosition, QStringLiteral("2"));
    QCOMPARE(commission->name, QStringLiteral("COMMISSION_PCT"));
    QCOMPARE(commission->columnType, QStringLiteral("NUMBER"));
    QCOMPARE(commission->columnNullable, QStringLiteral("YES"));
}

void tst_ObjectBrowserModel::applyRowsSetsTheTruncationIndicatorAndCount()
{
    ObjectBrowserModel model;
    model.setRowCap(2);
    auto group = makeNode(ObjectBrowserModel::NodeKind::Group);
    group->parent = model.m_root.get();
    ObjectBrowserModel::Node *groupPtr = group.get();
    model.m_root->children.push_back(std::move(group));

    const QVector<QStringList> rows = {
        { QStringLiteral("A"), {}, {}, {} },
        { QStringLiteral("B"), {}, {}, {} },
    };
    model.applyRows(groupPtr, rows, /* truncated = */ true);

    QVERIFY(groupPtr->truncated);
    QCOMPARE(groupPtr->shownCount, 2);
    const QModelIndex groupIdx = model.indexFor(groupPtr);
    const QString status = model.data(groupIdx, ObjectBrowserModel::StatusTextRole).toString();
    QVERIFY2(status.contains(QStringLiteral("2")), qPrintable(status));
    QVERIFY2(status.contains(QStringLiteral("more available")), qPrintable(status));
}

void tst_ObjectBrowserModel::setFilterIsANoOpForColumnsAndLeafNodes()
{
    ObjectBrowserModel model;
    auto columnsNode = makeNode(ObjectBrowserModel::NodeKind::ColumnsNode);
    columnsNode->parent = model.m_root.get();
    ObjectBrowserModel::Node *columnsPtr = columnsNode.get();
    model.m_root->children.push_back(std::move(columnsNode));

    const QModelIndex idx = model.indexFor(columnsPtr);
    model.setFilter(idx, QStringLiteral("abc"));
    QVERIFY(columnsPtr->filterText.isEmpty());
    QCOMPARE(model.filterFor(idx), QString());
}

void tst_ObjectBrowserModel::describeErrorNeverReturnsTheSameTextForPermissionAndOther()
{
    const QString permission = ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_PERMISSION);
    const QString other = ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_OTHER);
    const QString connectionLost = ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_NETWORK_LOST);
    const QString unknown = ObjectBrowserModel::describeError(RELDEX_ERROR_KIND_UNKNOWN);

    QVERIFY(!permission.isEmpty());
    QVERIFY(!other.isEmpty());
    QVERIFY(permission != other);
    QVERIFY(permission != connectionLost);
    // An unmapped/unknown kind still gets a safe, non-empty message (the same
    // fallback as any other unclassified failure) rather than an empty string
    // a view would render as a blank row.
    QCOMPARE(unknown, other);
}

void tst_ObjectBrowserModel::theRealClassifierTurnsAnAmbiguousOracleCodeIntoPermission()
{
    // Exercises the real, currently-working half of the metadata FFI family
    // (`reldex_metadata_prepare`/`_reclassify_error`) directly, independent
    // of any session -- see this file's header comment.
    ReldexMetadataRequest request = reldex::makeMetadataRequest();
    request.kind = RELDEX_METADATA_REQUEST_KIND_OBJECTS_OF_KIND;
    request.object_kind = RELDEX_METADATA_OBJECT_KIND_TABLES;
    const QByteArray schema = QByteArrayLiteral("HR");
    request.schema = ReldexStr { reinterpret_cast<const std::uint8_t *>(schema.constData()),
                                 static_cast<std::size_t>(schema.size()) };
    request.limit = 100;

    ReldexMetadataQuery *rawQuery = nullptr;
    QCOMPARE(reldex_metadata_prepare(&request, &rawQuery), RELDEX_STATUS_OK);
    QVERIFY(rawQuery != nullptr);
    const reldex::MetadataQueryHandle query(rawQuery);

    QCOMPARE(reldex_metadata_query_bind_count(query.get()), static_cast<std::size_t>(3));
    QCOMPARE(reldex_metadata_query_column_count(query.get()), static_cast<std::size_t>(4));

    const QByteArray message = QByteArrayLiteral("ORA-00942: table or view does not exist");
    const ReldexStr messageStr { reinterpret_cast<const std::uint8_t *>(message.constData()),
                                static_cast<std::size_t>(message.size()) };
    const reldex::ErrorHandle corrected(reldex_metadata_query_reclassify_error(
            query.get(), RELDEX_ERROR_KIND_SYNTAX, 942, true, messageStr));
    QVERIFY(corrected);
    ReldexErrorView view = reldex::makeErrorView();
    QCOMPARE(reldex_error_view(corrected.get(), &view), RELDEX_STATUS_OK);
    QCOMPARE(view.kind, RELDEX_ERROR_KIND_PERMISSION);
    QVERIFY(view.has_native);
    QCOMPARE(view.native_code, 942);

    // ORA-01031 ("insufficient privileges") is unconditionally Permission
    // already, for any SQL -- the classifier's job here is only to correct
    // the *ambiguous* codes, and it must not touch a code that needed no
    // correction into something else.
    const reldex::ErrorHandle unrelated(reldex_metadata_query_reclassify_error(
            query.get(), RELDEX_ERROR_KIND_PERMISSION, 1031, true, messageStr));
    QVERIFY(unrelated);
    ReldexErrorView unrelatedView = reldex::makeErrorView();
    QCOMPARE(reldex_error_view(unrelated.get(), &unrelatedView), RELDEX_STATUS_OK);
    QCOMPARE(unrelatedView.kind, RELDEX_ERROR_KIND_PERMISSION);
}

void tst_ObjectBrowserModel::applyingFiveThousandObjectsDoesNotBlockTheUiThreadForLong()
{
    // M6.1's "no freeze with a large schema" measurement, mock-data-only: the
    // real Oracle-container half cannot be reached at all from this build --
    // see `ObjectBrowserModel.h`'s top-of-file doc comment and
    // `ui/README.md` "Object browser (M6.1)" -> "The 'no freeze'
    // measurement" for exactly why (no `ReldexDriverKind` this FFI header
    // knows besides the mock).
    //
    // What this measures instead is real and exercises the actual freeze
    // risk the acceptance line cares about: `applyRows()` runs synchronously
    // on whatever thread calls it, which in real use is the Qt/UI thread
    // (`onMetadataResultComplete()` runs from `Bridge::dispatch()`, itself
    // called directly from the waker-triggered `drain()` slot on that
    // thread). Opt-in, like `tst_resultmodel`'s 1,000,000-row sanity test:
    // this is a recorded measurement, not a per-commit regression gate.
    if (adapter_test::envNumber("RELDEX_UI_SANITY_OBJECTS", 0) == 0) {
        QSKIP("set RELDEX_UI_SANITY_OBJECTS=1 to run the 5,000-object no-freeze measurement");
    }

    constexpr int kObjectCount = 5000;
    QVector<QStringList> rows;
    rows.reserve(kObjectCount);
    for (int i = 0; i < kObjectCount; ++i) {
        // Values differ per row (name, status cadence, both dates) so this is
        // not 5,000 copies of one row -- the same reason the brief asks for
        // that against a real schema.
        rows.append({
                QStringLiteral("TABLE_%1").arg(i, 5, 10, QLatin1Char('0')),
                (i % 7 == 0) ? QStringLiteral("INVALID") : QStringLiteral("VALID"),
                QStringLiteral("2020-%1-%2").arg((i % 12) + 1, 2, 10, QLatin1Char('0'))
                        .arg((i % 28) + 1, 2, 10, QLatin1Char('0')),
                QStringLiteral("2024-%1-%2").arg((i % 12) + 1, 2, 10, QLatin1Char('0'))
                        .arg((i % 28) + 1, 2, 10, QLatin1Char('0')),
        });
    }
    QVector<QStringList> filtered;
    for (const QStringList &row : rows) {
        if (row.constFirst().startsWith(QStringLiteral("TABLE_000"))) {
            filtered.append(row);
        }
    }

    ObjectBrowserModel model;
    auto group = makeNode(ObjectBrowserModel::NodeKind::Group);
    group->parent = model.m_root.get();
    group->schema = QStringLiteral("BIGSCHEMA");
    group->objectKind = RELDEX_METADATA_OBJECT_KIND_TABLES;
    ObjectBrowserModel::Node *groupPtr = group.get();
    model.m_root->children.push_back(std::move(group));

    constexpr int kRuns = 3;
    QVector<qint64> expandMs;
    QVector<qint64> filterMs;
    for (int run = 0; run < kRuns; ++run) {
        QElapsedTimer timer;
        timer.start();
        model.applyRows(groupPtr, rows, /* truncated = */ false); // "expand"
        expandMs.append(timer.elapsed());
        QCOMPARE(static_cast<int>(groupPtr->children.size()), kObjectCount);

        timer.restart();
        model.applyRows(groupPtr, filtered, /* truncated = */ false); // "filter"
        filterMs.append(timer.elapsed());
        QCOMPARE(static_cast<int>(groupPtr->children.size()), filtered.size());
    }

    for (int run = 0; run < kRuns; ++run) {
        qInfo("M6.1 no-freeze (mock, %d objects) run %d: expand %lld ms, filter-to-%lld %lld ms",
              kObjectCount, run + 1, static_cast<long long>(expandMs[run]),
              static_cast<long long>(filtered.size()), static_cast<long long>(filterMs[run]));
        // A loose sanity bound, not the measurement itself (recorded above
        // via qInfo -- see `phase-1.md` row M6.1 for the numbers this
        // produced and the machine state they were measured under): this
        // only catches a gross regression (an accidental O(n^2) path), not a
        // tight performance budget.
        QVERIFY2(expandMs[run] < 1000,
                 qPrintable(QStringLiteral("expand run %1 took %2 ms").arg(run).arg(expandMs[run])));
        QVERIFY2(filterMs[run] < 1000,
                 qPrintable(QStringLiteral("filter run %1 took %2 ms").arg(run).arg(filterMs[run])));
    }
}

void tst_ObjectBrowserModel::aFilterChangeWhileTheFirstFetchIsInFlightSupersedesRatherThanBeingDropped()
{
    // Regression test for the review finding on PR #47: `setFilter()`/
    // `expand(force = true)` used to be silently dropped by
    // `if (node->loading) return;` whenever a fetch for the same node was
    // already outstanding -- the node would settle showing the FIRST
    // filter's (by then stale) result while `filterFor()` reported the
    // SECOND, never-queried filter text, with no automatic recovery. Fixed
    // by letting `force` supersede an in-flight fetch instead of being
    // refused by it (`expand()`'s doc comment; `m_activeGeneration`).
    //
    // Wired under `QAbstractItemModelTester` per the review brief, to also
    // catch a begin/end-insert-rows mismatch from the extra child-reset this
    // exercises.
    Bridge bridge;
    QVERIFY(bridge.isValid());

    ObjectBrowserModel model;
    QAbstractItemModelTester tester(&model, QAbstractItemModelTester::FailureReportingMode::Fatal);
    model.setBridge(&bridge);
    const QModelIndex connection = model.index(0, 0);
    QVERIFY(connection.isValid());

    // Counts how many fetches actually reach `SessionController::Executing`
    // (i.e. `reldex_session_execute()` was actually called) -- friend access
    // to `m_metadataSession` reaches the model's own session directly, no
    // `findChild()` needed.
    int executingCount = 0;
    bool injected = false;

    model.setFilter(connection, QStringLiteral("abc")); // starts the first fetch
    QVERIFY(model.m_metadataSession != nullptr);
    connect(model.m_metadataSession, &SessionController::stateChanged, &model, [&] {
        if (model.m_metadataSession->state() == SessionController::Executing) {
            ++executingCount;
            if (!injected) {
                injected = true;
                // Mid-flight, synchronously, the instant the FIRST fetch is
                // submitted and strictly before any reply can have been
                // drained: exactly "the user changes the filter again before
                // the first fetch's reply lands".
                model.setFilter(connection, QStringLiteral("abcd"));
            }
        }
    });

    QVERIFY(spinUntil(
            [&] { return model.data(connection, ObjectBrowserModel::HasErrorRole).toBool(); }, 30000));
    QVERIFY(injected);
    // Let a second reply, if one is still in flight, land too.
    QVERIFY(spinUntil([&] { return executingCount >= 2; }, 5000));
    QTest::qWait(200);

    // A second fetch really was submitted (the superseding one, for "abcd") --
    // not just the first one's stale reply reinterpreted.
    QCOMPARE(executingCount, 2);
    // `filterFor()` never disagrees with what actually produced the
    // displayed (here: error) state once the node has settled.
    QCOMPARE(model.filterFor(connection), QStringLiteral("abcd"));
    QVERIFY(model.data(connection, ObjectBrowserModel::HasErrorRole).toBool());
    QVERIFY(!model.data(connection, ObjectBrowserModel::LoadingRole).toBool());
}

void tst_ObjectBrowserModel::destroyingTheModelWhileAFetchIsInFlightDoesNotCrash()
{
    // The model's destructor (`closeConnection()`) must never leave a
    // dangling `this`/`m_metadataSession` for a reply that lands after the
    // model itself is gone. Destroying `model` here, with a fetch genuinely
    // outstanding (no `spinUntil` wait for it to settle first), is exactly
    // the shape an ASan run (CI's `qt-asan` job) would catch a use-after-free
    // in.
    Bridge bridge;
    QVERIFY(bridge.isValid());

    {
        ObjectBrowserModel model;
        QAbstractItemModelTester tester(&model,
                                        QAbstractItemModelTester::FailureReportingMode::Fatal);
        model.setBridge(&bridge);
        model.expand(model.index(0, 0)); // starts opening the session / queues a fetch
        // Deliberately no wait: `model` (and `tester`) are destroyed right
        // here, with the session possibly still connecting and a fetch
        // possibly already in flight.
    }

    // Let whatever was still in flight for the now-destroyed model actually
    // finish draining on the hub side, so a use-after-free would have had its
    // chance to happen before the test process exits.
    QTest::qWait(200);
    spinUntil([] { return true; }, 200);
}

void tst_ObjectBrowserModel::destroyingTheBridgeBeforeTheModelDoesNotCrash()
{
    // Mirrors the ordering `QQmlApplicationEngine` teardown produced on CI's
    // `qt-asan` job (PR #47, tst_coreinfo): `Bridge` and the
    // `ObjectBrowserModel` that references it are unrelated QML-owned
    // siblings -- nothing parents one to the other, so nothing guarantees
    // which is destroyed first. Here `bridge` is destroyed deliberately
    // *before* `model`, while `model` (and the `SessionController` it owns)
    // is still alive and holding an open session against it.
    // `~ObjectBrowserModel()` then runs `closeConnection()` ->
    // `performClose()` -> `SessionController::closeSession()`, which used to
    // dereference `m_bridge` as a plain, already-dangling `Bridge *` -- the
    // exact call the ASan report pointed at (`SessionController.cpp`,
    // `closeSession()`, called from `ObjectBrowserModel::closeConnection()`,
    // called from `~ObjectBrowserModel()`). `QPointer<Bridge>` is what must
    // turn that into a no-op instead of a use-after-free.
    auto *bridge = new Bridge();
    QVERIFY(bridge->isValid());

    {
        ObjectBrowserModel model;
        QAbstractItemModelTester tester(&model,
                                        QAbstractItemModelTester::FailureReportingMode::Fatal);
        model.setBridge(bridge);
        model.expand(model.index(0, 0)); // opens the model's own metadata session

        // Wait for the session to actually finish opening: `closeConnection()`
        // defers via `m_closePending` -- never reaching `SessionController::
        // closeSession()` at all -- until `m_sessionReady` is true, so
        // destroying `bridge` any earlier would exercise nothing.
        QVERIFY(spinUntil([&model] { return model.m_sessionReady; }, 5000));

        delete bridge; // exactly what QQmlApplicationEngine teardown can do first

        // `model` (and `tester`) are destroyed right here, with its
        // `SessionController` now pointing at a `Bridge` that no longer
        // exists. Must not crash / access-violate.
    }

    QTest::qWait(200);
}

QTEST_GUILESS_MAIN(tst_ObjectBrowserModel)

#include "tst_objectbrowsermodel.moc"
