#pragma once

// `ResultTableModel` -- ADR-0003 D4's virtualized grid model.
//
// It owns the fetched `ReldexBatch*`s and reads cells out of the memory they
// borrow to it. Per ADR-0003 D4 and `AGENTS.md`: never one object per row, and
// `data()` never calls an FFI function beyond the pointer reads the column
// views already gave it -- the one exception is the *documented lazy bulk
// format path* (`hydrate()`), which runs at most once per (batch, column)
// while that batch is cached, not once per cell.

#include "ReldexHandles.h"

#include <QAbstractTableModel>
#include <QByteArray>
#include <QString>
#include <QStringList>
#include <QVector>
#include <QtQml/qqmlregistration.h>

#include <deque>
#include <vector>

/// Where `fetchMore()` goes. An interface rather than a direct dependency on
/// `SessionController` so the model can be driven by a stub in a test
/// (ADR-0003 D10) and so the model holds no session state of its own.
class ResultFetchSource
{
public:
    ResultFetchSource() = default;
    virtual ~ResultFetchSource() = default;
    ResultFetchSource(const ResultFetchSource &) = delete;
    ResultFetchSource &operator=(const ResultFetchSource &) = delete;

    [[nodiscard]] virtual bool canFetchMoreRows() const = 0;
    virtual void fetchMoreRows() = 0;
};

class ResultTableModel : public QAbstractTableModel
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ResultTableModel is created by SessionController and reached as session.model")

    /// How many batches may keep formatted text at once. The row data of every
    /// fetched batch is retained (ADR-0003 D4: the MVP keeps the fetched
    /// prefix); the *formatted* text is not -- see `setMaxCachedBatches`.
    Q_PROPERTY(int maxCachedBatches READ maxCachedBatches WRITE setMaxCachedBatches NOTIFY
                       maxCachedBatchesChanged)
    /// What a SQL NULL renders as. A grid must also *visualize* NULL rather
    /// than rely on this text (`SPEC.md` §24) -- that is what `isNull` is for.
    Q_PROPERTY(QString nullText READ nullText WRITE setNullText NOTIFY nullTextChanged)
    /// A `ReldexTimestampStyle`.
    Q_PROPERTY(int timestampStyle READ timestampStyle WRITE setTimestampStyle NOTIFY
                       timestampStyleChanged)
    Q_PROPERTY(int batchCount READ batchCount NOTIFY batchCountChanged)
    /// The column names, once the result's first batch has arrived (the count
    /// comes from the EXECUTED event, the names from a batch -- ADR-0003 A6).
    ///
    /// A notifying property rather than QML calling `headerData()`: a binding
    /// on `headerData()` would evaluate once, before the names existed, and
    /// never re-evaluate, because `headerDataChanged` is not a property-change
    /// signal. Found by the QML test, not by reasoning.
    Q_PROPERTY(QStringList columnNames READ columnNames NOTIFY columnNamesChanged)

public:
    enum Roles {
        /// `true` when the cell is SQL NULL. NULL, empty and taken are three
        /// different states and only this role distinguishes them.
        IsNullRole = Qt::UserRole + 1,
    };
    Q_ENUM(Roles)

    explicit ResultTableModel(QObject *parent = nullptr);
    ~ResultTableModel() override;

    // --- QAbstractTableModel ----------------------------------------------
    [[nodiscard]] int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = QModelIndex()) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                      int role = Qt::DisplayRole) const override;
    [[nodiscard]] QHash<int, QByteArray> roleNames() const override;
    [[nodiscard]] bool canFetchMore(const QModelIndex &parent) const override;
    void fetchMore(const QModelIndex &parent) override;

    // --- driven by SessionController ---------------------------------------

    void setFetchSource(ResultFetchSource *source);

    /// Starts a new result set with `columns` columns and no rows yet.
    void beginResult(int columns);

    /// Takes one fetched batch. A batch with no rows is released immediately
    /// (it only means the result is exhausted); its column names are still
    /// read first if they are not known yet, so an empty result still has
    /// headers.
    void applyBatch(reldex::BatchHandle batch, int rows);

    /// Releases every batch and every cached arena, and empties the model.
    void reset();

    [[nodiscard]] int maxCachedBatches() const noexcept { return m_maxCachedBatches; }
    void setMaxCachedBatches(int batches);
    [[nodiscard]] QString nullText() const { return m_nullText; }
    void setNullText(const QString &text);
    [[nodiscard]] int timestampStyle() const noexcept { return m_timestampStyle; }
    void setTimestampStyle(int style);
    [[nodiscard]] int batchCount() const noexcept { return static_cast<int>(m_batches.size()); }
    [[nodiscard]] QStringList columnNames() const;

    /// How many (batch, column) pairs currently hold formatted text. Test and
    /// diagnostics only.
    [[nodiscard]] int hydratedColumnCount() const;

Q_SIGNALS:
    void maxCachedBatchesChanged();
    void nullTextChanged();
    void timestampStyleChanged();
    void batchCountChanged();
    void columnNamesChanged();

private:
    struct FormattedColumn
    {
        reldex::ArenaHandle arena;
        ReldexArenaView view {};
    };

    struct BatchEntry
    {
        reldex::BatchHandle handle;
        int firstRow = 0;
        int rows = 0;
        /// One view per column, obtained **once** per batch, never per cell.
        std::vector<ReldexColumnView> views;
        /// Lazily filled for columns the formatter has to render.
        std::vector<FormattedColumn> formatted;
    };

    struct ColumnHeader
    {
        QString name;
        qint32 kind = RELDEX_COLUMN_KIND_UNKNOWN;
    };

    void releaseEverything();
    void readColumnHeaders(const ReldexBatch *batch);
    [[nodiscard]] int batchForRow(int row) const;
    [[nodiscard]] QString formattedCell(int batchIndex, int column, int localRow) const;
    void hydrate(int batchIndex, int column) const;
    void touch(int batchIndex) const;
    void dropFormattedText(int batchIndex) const;
    void dropAllFormattedText() const;
    void evictIfNeeded() const;
    void reformatEverything();
    void rebuildFormatOptions();

    ResultFetchSource *m_source = nullptr;

    /// `mutable` because the formatted-text cache is filled lazily from the
    /// const `data()` path; the row data itself is only ever appended from
    /// `applyBatch()`.
    mutable std::deque<BatchEntry> m_batches;
    /// `m_firstRows[i] == m_batches[i].firstRow`, so a row lookup is one
    /// `upper_bound` -- O(log n) with no per-row object anywhere.
    std::vector<int> m_firstRows;
    QVector<ColumnHeader> m_columns;

    int m_rowCount = 0;
    int m_columnCount = 0;

    /// Sequential access (which is what scrolling is) hits this before the
    /// binary search.
    mutable int m_lastBatch = -1;
    /// Least-recently-used first. Only batches with formatted text are in it.
    mutable std::vector<int> m_hydrationOrder;

    int m_maxCachedBatches = 16;
    QString m_nullText = QStringLiteral("(null)");
    int m_timestampStyle = RELDEX_TIMESTAMP_STYLE_ISO8601;

    /// Rebuilt whenever a string it points into changes; the UTF-8 buffers
    /// must outlive every call that reads them.
    ReldexFormatOptions m_formatOptions {};
    QByteArray m_nullTextUtf8;
};
