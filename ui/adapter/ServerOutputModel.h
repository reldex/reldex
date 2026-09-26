#pragma once

// `ServerOutputModel` -- M4.7: the DBMS_OUTPUT pane's virtualized line list.
//
// ARCHITECTURE.md invariant "never one QML object per row" and AGENTS.md's
// "large results must be cursor/batch based and virtualized" apply just as
// much to a text pane as to a result grid: a session with output on can
// accumulate a very large number of lines over a long-running script, so this
// is a `QAbstractListModel` meant to sit behind a `ListView` (delegate reuse,
// `reuseItems: true`) rather than a `Repeater`/`ListModel`, which would create
// one QML `Text` item per line up front. `data()` is a plain vector index --
// no formatting, no FFI call, no lazy hydration -- because a `DBMS_OUTPUT`
// line is already a short, plain UTF-8 string by the time
// `SessionController::serverOutputReceived()` hands it here (M2.12 decoded
// it, replacing invalid bytes with U+FFFD; `ServerOutputController` counts
// how many).
//
// Ownership: lines are copied in (`QString`, already owned), never a view
// into FFI memory -- unlike `ResultTableModel`, nothing here borrows from a
// `ReldexBatch`/`ReldexServerOutputLines` past the call that hands them over,
// so there is no batch-release lifetime to manage.

#include <QAbstractListModel>
#include <QStringList>
#include <QtQml/qqmlregistration.h>

#include <vector>

class ServerOutputModel : public QAbstractListModel
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ServerOutputModel is created by ServerOutputController and reached as "
                     "serverOutput.model")

public:
    enum Roles {
        /// One line's text, with no trailing newline (`DBMS_OUTPUT.PUT_LINE`'s
        /// own unit).
        LineTextRole = Qt::UserRole + 1,
    };
    Q_ENUM(Roles)

    explicit ServerOutputModel(QObject *parent = nullptr);

    [[nodiscard]] int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QHash<int, QByteArray> roleNames() const override;

    /// Appends one event's worth of lines (or a test's synthetic batch) in one
    /// `beginInsertRows`/`endInsertRows` pair, in the order given -- callers
    /// (`ServerOutputController`) already receive lines in arrival order
    /// (ADR-0003 A32's per-session production order), so this never reorders.
    /// A no-op for an empty list (no reset signal for nothing changing).
    void appendLines(const QStringList &lines);

    /// The pane's Clear button (M4.7): empties the model. Local UI state only
    /// -- never touches the settings registry or the session's own server-side
    /// buffer (the user's `DISABLE`/next `ENABLE` governs that, not this).
    Q_INVOKABLE void clear();

private:
    std::vector<QString> m_lines;
};
