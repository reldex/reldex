#pragma once

// `ResultTableModel` -- ADR-0003 D4's virtualized grid model.
//
// It owns the fetched `ReldexBatch*`s and reads cells out of the memory they
// borrow to it. Per ADR-0003 D4 and `AGENTS.md`: never one object per row, and
// `data()` never calls an FFI function beyond the pointer reads the column
// views already gave it -- the one exception is the *documented lazy bulk
// format path* (`hydrate()`), which renders one bounded **window** of one
// column, not a whole batch and never one cell.
//
// The window is what makes the cost independent of the fetch size. D4 asks for
// "one call per visible window"; formatting a whole batch happens to be that
// when a batch is 1,000 rows and is emphatically not when it is 50,000 --
// measured during this task's review at 2.74 ms (NUMBER) and 7.55 ms (DATE)
// for the first cell read out of a 50,000-row batch, which is a dropped frame
// the boundary would get blamed for. `kWindowRows` bounds it whatever
// `fetchRows` is set to.

#include "ReldexHandles.h"

#include <QAbstractTableModel>
#include <QByteArray>
#include <QHash>
#include <QString>
#include <QStringList>
#include <QList>
#include <QtQml/qqmlregistration.h>

#include <cstddef>
#include <deque>
#include <limits>
#include <list>
#include <unordered_map>
#include <utility>
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

    /// How many formatted windows may be cached at once. The row data of every
    /// fetched batch is retained (ADR-0003 D4: the MVP keeps the fetched
    /// prefix); the *formatted* text is not.
    Q_PROPERTY(int maxFormattedWindows READ maxFormattedWindows WRITE setMaxFormattedWindows NOTIFY
                       formattedBoundChanged)
    /// The other half of the bound: how many bytes of formatted text may be
    /// cached. Whichever limit bites first evicts, except that the window just
    /// rendered is always kept, so a single oversized window cannot make the
    /// cache useless.
    Q_PROPERTY(qint64 maxFormattedBytes READ maxFormattedBytes WRITE setMaxFormattedBytes NOTIFY
                       formattedBoundChanged)
    /// How many rows one formatted window covers. Fixed, because it is a
    /// latency bound rather than a preference.
    Q_PROPERTY(int formatWindowRows READ formatWindowRows CONSTANT)
    /// The model refuses rows past this. `QAbstractItemModel` counts rows in
    /// `int`, so `INT_MAX` is a hard ceiling whatever this says; writable so
    /// the refusal path is testable without 2^31 rows.
    Q_PROPERTY(int maxRows READ maxRows WRITE setMaxRows NOTIFY maxRowsChanged)
    /// True once the formatter has failed for some cell. The failure is logged
    /// once, not once per repaint, and the affected cells render empty.
    Q_PROPERTY(bool formattingFailed READ formattingFailed NOTIFY formattingFailedChanged)
    /// True once a batch was refused because `maxRows` was reached. Fetching
    /// stops; what was already fetched stays readable.
    Q_PROPERTY(bool rowLimitReached READ rowLimitReached NOTIFY rowLimitReachedChanged)
    /// What a SQL NULL renders as. A grid must also *visualize* NULL rather
    /// than rely on this text (`SPEC.md` §24) -- that is what `isNull` is for.
    Q_PROPERTY(QString nullText READ nullText WRITE setNullText NOTIFY nullTextChanged)
    /// A `ReldexTimestampStyle`.
    Q_PROPERTY(int timestampStyle READ timestampStyle WRITE setTimestampStyle NOTIFY
                       timestampStyleChanged)
    Q_PROPERTY(int batchCount READ batchCount NOTIFY batchCountChanged)
    /// The column names of the current result.
    ///
    /// A notifying property rather than QML calling `headerData()`, because a
    /// QML binding on `headerData()` never re-evaluates: `headerDataChanged`
    /// is a model signal, not a property-change signal, so a header row bound
    /// that way keeps whatever it read on the previous result.
    Q_PROPERTY(QStringList columnNames READ columnNames NOTIFY columnNamesChanged)

public:
    /// One column of a result, as the statement itself described it.
    ///
    /// ABI 3: `SessionController` reads these from
    /// `reldex_session_result_column` the moment the `EXECUTED` event is
    /// drained, so the model gets its count *and* its names in one step,
    /// before any row exists. Before ABI 3 the count came from the event and
    /// the names from the first batch, which meant a two-phase header, a
    /// late `headerDataChanged`, a reset if the two ever disagreed -- and no
    /// headers at all for a result with columns and no rows.
    struct ColumnDescription
    {
        QString name;
        qint32 kind = RELDEX_COLUMN_KIND_UNKNOWN;
    };

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

    /// Starts a new result set with these columns and no rows yet. An empty
    /// list is "no result": zero rows, zero columns.
    void beginResult(const QList<ColumnDescription> &columns);

    /// Takes one fetched batch. A batch with no rows is released immediately:
    /// it is the terminal marker, it has no rows *and no columns* (ABI 3), so
    /// there is nothing on it to read.
    void applyBatch(reldex::BatchHandle batch, int rows);

    /// Releases every batch and every cached arena, and empties the model.
    void reset();

    [[nodiscard]] int maxFormattedWindows() const noexcept { return m_maxFormattedWindows; }
    void setMaxFormattedWindows(int windows);
    [[nodiscard]] qint64 maxFormattedBytes() const noexcept { return m_maxFormattedBytes; }
    void setMaxFormattedBytes(qint64 bytes);
    [[nodiscard]] static constexpr int formatWindowRows() noexcept { return kWindowRows; }
    [[nodiscard]] int maxRows() const noexcept { return m_maxRows; }
    void setMaxRows(int rows);
    [[nodiscard]] bool formattingFailed() const noexcept { return m_formattingFailed; }
    [[nodiscard]] bool rowLimitReached() const noexcept { return m_rowLimitReached; }
    [[nodiscard]] QString nullText() const { return m_nullText; }
    void setNullText(const QString &text);
    [[nodiscard]] int timestampStyle() const noexcept { return m_timestampStyle; }
    void setTimestampStyle(int style);
    [[nodiscard]] int batchCount() const noexcept { return static_cast<int>(m_batches.size()); }
    [[nodiscard]] QStringList columnNames() const;

    /// How many formatted windows are cached right now. Test and diagnostics.
    [[nodiscard]] int formattedWindowCount() const noexcept
    {
        return static_cast<int>(m_windows.size());
    }
    /// How many bytes of formatted text are cached right now.
    [[nodiscard]] qint64 formattedBytes() const noexcept { return m_formattedBytes; }

Q_SIGNALS:
    void formattedBoundChanged();
    void maxRowsChanged();
    void formattingFailedChanged();
    void rowLimitReachedChanged();
    void nullTextChanged();
    void timestampStyleChanged();
    void batchCountChanged();
    void columnNamesChanged();

private:
    /// Rows per formatted window. 1,024 because it is comfortably more than a
    /// viewport holds at any sane row height (so a scroll rarely crosses a
    /// window boundary mid-frame) and small enough that rendering one is tens
    /// of microseconds for the S14 shape.
    static constexpr int kWindowRows = 1024;

    /// One rendered (batch, column, window). Keyed, LRU-ordered, and bounded
    /// by both a count and a byte total.
    struct FormattedWindow
    {
        reldex::ArenaHandle arena;
        ReldexArenaView view {};
        /// First **local** row of the batch this window covers.
        int firstLocalRow = 0;
        /// Approximate bytes this window holds, for the byte bound.
        qint64 bytes = 0;
        /// The formatter refused this window. Remembered so `data()` does not
        /// retry -- and therefore call FFI -- on every repaint of every cell.
        bool failed = false;
    };

    /// What a cached window is keyed by. A struct rather than three numbers
    /// packed into a `quint64`: any packing has to bound one of the three, and
    /// the failure mode of guessing that bound wrong is two different windows
    /// colliding on one key -- which shows up as *wrong cell text*, the kind of
    /// bug a grid makes very hard to notice.
    struct WindowKey
    {
        int batch = 0;
        int column = 0;
        int window = 0;

        [[nodiscard]] bool operator==(const WindowKey &other) const noexcept
        {
            return batch == other.batch && column == other.column && window == other.window;
        }
    };

    /// Kept nested (rather than a `qHash` overload) so the key type can stay
    /// private: a free `qHash` would have to be findable by ADL, and therefore
    /// public.
    struct WindowKeyHash
    {
        [[nodiscard]] std::size_t operator()(const WindowKey &key) const noexcept
        {
            // A collision here costs a comparison, not a wrong answer.
            std::size_t hash = static_cast<std::size_t>(static_cast<unsigned>(key.batch));
            hash = hash * 1000003U + static_cast<std::size_t>(static_cast<unsigned>(key.column));
            hash = hash * 1000003U + static_cast<std::size_t>(static_cast<unsigned>(key.window));
            return hash;
        }
    };

    struct BatchEntry
    {
        reldex::BatchHandle handle;
        int firstRow = 0;
        int rows = 0;
        /// One view per column, obtained **once** per batch, never per cell.
        std::vector<ReldexColumnView> views;
    };

    /// The LRU list holds the windows; the index maps a key to its node. A
    /// `std::list` because neither eviction nor re-touching may invalidate the
    /// iterators the index stores.
    using WindowList = std::list<std::pair<WindowKey, FormattedWindow>>;

    void releaseEverything();
    [[nodiscard]] int batchForRow(int row) const;
    [[nodiscard]] QString formattedCell(int batchIndex, int column, int localRow) const;
    /// Renders one window; returns the cache entry, or nullptr if the window is
    /// empty. Never called more than once per (batch, column, window) while
    /// that entry stays cached.
    const FormattedWindow *hydrate(const WindowKey &key, int rowsInBatch,
                                   const ReldexBatch *batch) const;
    void touch(WindowList::iterator node) const;
    void evictIfNeeded(const WindowKey &keep) const;
    void dropAllFormattedText() const;
    void reformatEverything();
    void rebuildFormatOptions();
    void noteFormatFailure(const char *what) const;

    ResultFetchSource *m_source = nullptr;

    /// `mutable` because the formatted-text cache is filled lazily from the
    /// const `data()` path; the row data itself is only ever appended from
    /// `applyBatch()`.
    mutable std::deque<BatchEntry> m_batches;
    /// `m_firstRows[i] == m_batches[i].firstRow`, so a row lookup is one
    /// `upper_bound` -- O(log n) with no per-row object anywhere.
    std::vector<int> m_firstRows;
    /// The whole truth about this result's columns, and the *only* source of
    /// `columnCount()`: with ABI 3 there is no window in which the count and
    /// the names can disagree, so there is no longer a second number to keep
    /// in step with this one.
    QList<ColumnDescription> m_columns;

    int m_rowCount = 0;

    /// Sequential access (which is what scrolling is) hits this before the
    /// binary search.
    mutable int m_lastBatch = -1;

    /// The formatted-text cache: an LRU list, most-recently-used at the front,
    /// plus a key -> node index.
    mutable WindowList m_lru;
    mutable std::unordered_map<WindowKey, WindowList::iterator, WindowKeyHash> m_windows;
    mutable qint64 m_formattedBytes = 0;

    /// 64 windows x 1,024 rows is ~65,000 rows of rendered text -- far more
    /// than any viewport, and for the S14 shape about 1.2 MB. The byte bound
    /// is what actually holds when a column is wide.
    int m_maxFormattedWindows = 64;
    qint64 m_maxFormattedBytes = 8LL * 1024 * 1024;

    int m_maxRows = std::numeric_limits<int>::max();
    mutable bool m_formattingFailed = false;
    bool m_rowLimitReached = false;

    QString m_nullText = QStringLiteral("(null)");
    int m_timestampStyle = RELDEX_TIMESTAMP_STYLE_ISO8601;

    /// Rebuilt whenever a string it points into changes; the UTF-8 buffers
    /// must outlive every call that reads them.
    ReldexFormatOptions m_formatOptions {};
    QByteArray m_nullTextUtf8;
};
