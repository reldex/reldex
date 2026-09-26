#pragma once

// Shared helpers for the M1.6 adapter tests.
//
// Header-only on purpose: three small test binaries do not need a library
// target, and every helper here is either a spin loop or a re-statement of
// what the mock driver generates.

#include <QByteArray>
#include <QCoreApplication>
#include <QDate>
#include <QDeadlineTimer>
#include <QEventLoop>
#include <QString>
#include <QThread>
#include <QtGlobal>
#include <QtQuick/QQuickItem>

#include <Bridge.h>
#include <ResultTableModel.h>
#include <SessionController.h>

namespace adapter_test {

// M3.2 fix round, must-fix 1(b) (2026-09-26): a harness-level default so a
// future test cannot forget to force the workspace in-memory. This header is
// included by every UI test's own .cpp (tst_bridge, tst_resultmodel,
// tst_teardown, tst_connectionmanager, and tst_coreinfo -- which loads the
// real Main.qml, and therefore a real Bridge/ConnectionManager whose
// Component.onCompleted calls connections.open()). A namespace-scope
// `inline const` object's constructor runs during static initialization,
// before QTEST_MAIN/QTEST_GUILESS_MAIN's generated main() constructs
// anything or loads any QML -- so RELDEX_WORKSPACE_IN_MEMORY is already "1"
// by the time anything in this process could call
// ConnectionManager::open(). This is the outermost of three layers (the
// others: ConnectionManager itself never opens automatically; ui/build.sh
// --test asserts afterward that the real default store was not touched) --
// belt, suspenders, and a smoke alarm.
//
// tst_connectionmanager's own `aRealOnDiskWorkspaceRoundTripsThroughTheResolvedDefaultPath`
// test unsets this itself, deliberately, after this constructor has already
// run -- it is the one test that wants the real (but redirected, via a
// QTemporaryDir standing in for the platform data directory) on-disk path.
struct ForceInMemoryWorkspaceForTests
{
    ForceInMemoryWorkspaceForTests()
    {
        qputenv("RELDEX_WORKSPACE_IN_MEMORY", "1");
        qputenv("RELDEX_WORKSPACE_MEMORY_CREDENTIAL_STORE", "1");
    }
};
inline const ForceInMemoryWorkspaceForTests forceInMemoryWorkspaceForTests;

/// Finds a descendant by `objectName`, walking the **visual** item tree
/// (`QQuickItem::childItems()`) rather than `QObject::findChild()`'s
/// `QObject::children()`.
///
/// M3.1 finding: a `Repeater`/`ListView`/`GridView` delegate is reparented
/// into the scene *visually* (`setParentItem`) but is not necessarily a
/// `QObject` child of anything in that visual chain -- `ui/README.md`
/// already documents this for `Repeater` ("a Repeater owns its delegates and
/// only re-parents them *visually*"), and it holds for `ListView` too:
/// `QObject::findChild()` silently does not see into a `ListView`'s
/// delegates at all, with no warning and no error, just a null result. Use
/// this instead of `findChild<QQuickItem *>()` for anything that might be
/// produced by one of those.
inline QQuickItem *findVisualChild(QQuickItem *root, const QString &name)
{
    if (root == nullptr) {
        return nullptr;
    }
    const QList<QQuickItem *> children = root->childItems();
    for (QQuickItem *child : children) {
        if (child->objectName() == name) {
            return child;
        }
    }
    for (QQuickItem *child : children) {
        if (QQuickItem *found = findVisualChild(child, name)) {
            return found;
        }
    }
    return nullptr;
}

/// Runs the event loop until `predicate()` holds, or the deadline passes.
///
/// Never asserts an upper bound on latency: the deadline is a hang guard, so a
/// failure reads as "this never happened" rather than "this was too slow".
///
/// Yields rather than sleeps between turns. `QThread::usleep` on Windows is
/// `Sleep(us / 1000 + 1)`, which the default ~15.6 ms timer resolution rounds
/// up -- with three waits per iteration that put K5's 10,000 teardowns at
/// about eight minutes of almost pure sleeping. `yieldCurrentThread()` has no
/// such floor.
template<typename Predicate>
bool spinUntil(Predicate predicate, int timeoutMs = 60000)
{
    QDeadlineTimer deadline(timeoutMs);
    while (!predicate()) {
        QCoreApplication::processEvents(QEventLoop::AllEvents, 2);
        if (predicate()) {
            return true;
        }
        if (deadline.hasExpired()) {
            return false;
        }
        QThread::yieldCurrentThread();
    }
    return true;
}

// --- ABI 3 live-object counts (reldex_live_counts) -------------------------
//
// The leak check this suite could not make before. Counts are process-wide and
// include what is not the caller's yet: a batch inside an undrained event is
// live, and so is the error in a thread's last-error slot.

struct LiveCounts
{
    qint64 hubs = 0;
    qint64 sessions = 0;
    qint64 batches = 0;
    qint64 errors = 0;
    qint64 arenas = 0;

    [[nodiscard]] bool operator==(const LiveCounts &other) const noexcept
    {
        return hubs == other.hubs && sessions == other.sessions && batches == other.batches
                && errors == other.errors && arenas == other.arenas;
    }

    [[nodiscard]] QString toString() const
    {
        return QStringLiteral("hubs=%1 sessions=%2 batches=%3 errors=%4 arenas=%5")
                .arg(hubs)
                .arg(sessions)
                .arg(batches)
                .arg(errors)
                .arg(arenas);
    }
};

inline LiveCounts liveCounts()
{
    ReldexLiveCounts raw = reldex::makeLiveCounts();
    if (reldex_live_counts(&raw) != RELDEX_STATUS_OK) {
        return {};
    }
    return LiveCounts {
        static_cast<qint64>(raw.hubs),   static_cast<qint64>(raw.sessions),
        static_cast<qint64>(raw.batches), static_cast<qint64>(raw.errors),
        static_cast<qint64>(raw.arenas),
    };
}

/// Waits for the live counts to come back to `baseline`.
///
/// A wait rather than an immediate compare. Since M2.15 the library releases
/// everything it counts before `reldex_hub_destroy` returns, so the wait is a
/// guard rather than a necessity; it costs nothing when the counts are
/// already right. The deadline is a hang guard, never a latency bound.
inline bool spinUntilLiveCounts(const LiveCounts &baseline, int timeoutMs = 60000)
{
    return spinUntil([&baseline] { return liveCounts() == baseline; }, timeoutMs);
}

/// Waits for the library to go quiescent, and returns that as a baseline.
///
/// Take a baseline with this rather than with `liveCounts()` directly. The
/// counts are **process-wide**, and a previous test's objects can still be
/// on their way out when the next one starts -- a baseline read in that
/// window records a hub and a session that are about to go, and the test then
/// fails at the end for having *fewer* live objects than it started with.
/// Found exactly that way, not by reasoning.
///
/// The calling thread's last-error slot is emptied first. An error sitting
/// there counts as live exactly like one the caller holds, so a baseline taken
/// over the top of a previous test's failure is a number the test can only
/// drop below -- which reads as a leak in reverse.
inline LiveCounts settledBaseline(int timeoutMs = 60000)
{
    const reldex::ErrorHandle stale(reldex_last_error_take());
    Q_UNUSED(stale);
    spinUntil(
            [] {
                const LiveCounts counts = liveCounts();
                return counts.hubs == 0 && counts.sessions == 0 && counts.batches == 0
                        && counts.arenas == 0;
            },
            timeoutMs);
    return liveCounts();
}

inline qint64 envNumber(const char *name, qint64 fallback)
{
    const QByteArray value = qgetenv(name);
    if (value.isEmpty()) {
        return fallback;
    }
    bool ok = false;
    const qint64 parsed = value.toLongLong(&ok);
    return ok ? parsed : fallback;
}

// --- what crates/drivers/mock/src/generated.rs generates -------------------
//
// Restated here rather than read from the mock, because the point of the check
// is that the *model* reports what the mock produced, all the way through the
// boundary. Written with explicit scalar values so the file's own encoding
// cannot be what a failure is about.

inline QString fromScalars(const char32_t *scalars, qsizetype count)
{
    return QString::fromUcs4(scalars, count);
}

/// `THAI_WORDS` in the mock: ข้อมูล, ทดสอบ, แถว, ฐานข้อมูล.
inline QString thaiWord(int index)
{
    static const char32_t w0[] = { 0x0E02, 0x0E49, 0x0E2D, 0x0E21, 0x0E39, 0x0E25 };
    static const char32_t w1[] = { 0x0E17, 0x0E14, 0x0E2A, 0x0E2D, 0x0E1A };
    static const char32_t w2[] = { 0x0E41, 0x0E16, 0x0E27 };
    static const char32_t w3[] = { 0x0E10, 0x0E32, 0x0E19, 0x0E02, 0x0E49,
                                   0x0E2D, 0x0E21, 0x0E39, 0x0E25 };
    switch (index) {
    case 0:
        return fromScalars(w0, 6);
    case 1:
        return fromScalars(w1, 5);
    case 2:
        return fromScalars(w2, 3);
    default:
        return fromScalars(w3, 9);
    }
}

/// `EMOJI_GLYPHS` in the mock, all non-BMP: U+1F680, U+1F389, U+1F40D, U+1F9E9.
inline QString emojiGlyph(int index)
{
    static const char32_t glyphs[] = { 0x1F680, 0x1F389, 0x1F40D, 0x1F9E9 };
    const char32_t glyph = glyphs[index & 3];
    return fromScalars(&glyph, 1);
}

/// The mock's `pad40`: pad with '.' to 40 **Unicode scalars**, like RPAD, and
/// leave anything already at or past 40 alone.
inline QString pad40(const QString &text)
{
    qsizetype scalars = 0;
    for (qsizetype index = 0; index < text.size(); ++index) {
        if (!text.at(index).isLowSurrogate()) {
            ++scalars;
        }
    }
    QString padded = text;
    for (qsizetype index = scalars; index < 40; ++index) {
        padded.append(QLatin1Char('.'));
    }
    return padded;
}

inline int pick(quint64 rowNumber, quint64 seed, int length)
{
    return static_cast<int>((rowNumber + seed) % static_cast<quint64>(length));
}

/// The mock's NULL cadence: every 100th row of NAME.
inline bool expectedNameIsNull(quint64 rowNumber)
{
    return rowNumber % 100 == 0;
}

/// NAME for the 1-based `rowNumber`, checked from the narrowest cadence down
/// exactly as the mock does (100 wins over 25 and 10).
inline QString expectedName(quint64 rowNumber, quint64 seed)
{
    if (expectedNameIsNull(rowNumber)) {
        return {};
    }
    if (rowNumber % 25 == 0) {
        return pad40(QStringLiteral("row %1 %2")
                             .arg(rowNumber)
                             .arg(emojiGlyph(pick(rowNumber, seed, 4))));
    }
    if (rowNumber % 10 == 0) {
        return pad40(QStringLiteral("%1 %2").arg(thaiWord(pick(rowNumber, seed, 4))).arg(rowNumber));
    }
    return pad40(QStringLiteral("row %1").arg(rowNumber));
}

/// ID: the 1-based row number, rendered by the bulk formatter with the default
/// options (plain positional decimal, no grouping).
inline QString expectedId(quint64 rowNumber)
{
    return QString::number(rowNumber);
}

/// CREATED: 2026-01-01 plus `rowNumber` days, in `DATE_ONLY` style.
inline QString expectedCreatedDateOnly(quint64 rowNumber)
{
    return QDate(2026, 1, 1).addDays(static_cast<qint64>(rowNumber)).toString(Qt::ISODate);
}

/// Opens the mock session and runs the generated query, then waits for the
/// whole result to stream in.
inline bool streamGeneratedQuery(Bridge &bridge, qint64 rows, int fetchRows, int fetchesInFlight,
                                 qint64 seed = 0, int timeoutMs = 120000)
{
    SessionController *session = bridge.session();
    if (session == nullptr) {
        return false;
    }
    session->setMockRows(rows);
    session->setMockSeed(seed);
    session->setFetchRows(fetchRows);
    session->setMaxFetchesInFlight(fetchesInFlight);
    session->setRunOnOpen(true);
    if (!session->open()) {
        return false;
    }
    return spinUntil(
            [session] {
                return session->state() == SessionController::ResultComplete
                        || session->state() == SessionController::Failed;
            },
            timeoutMs);
}

} // namespace adapter_test
