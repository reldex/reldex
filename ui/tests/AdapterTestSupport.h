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

#include <Bridge.h>
#include <ResultTableModel.h>
#include <SessionController.h>

namespace adapter_test {

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
