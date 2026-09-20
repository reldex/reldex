#include "ResultTableModel.h"

#include <QtGlobal>

#include <algorithm>

namespace {

/// The one place a formatted cell's bytes are turned into a `QString`.
QString stringFromArena(const ReldexArenaView &view, std::size_t index)
{
    if (view.data == nullptr || view.offsets == nullptr || index >= view.count) {
        return {};
    }
    const std::size_t begin = view.offsets[index];
    const std::size_t end = view.offsets[index + 1];
    if (end < begin || end > view.data_len) {
        return {};
    }
    return QString::fromUtf8(reinterpret_cast<const char *>(view.data + begin),
                             static_cast<qsizetype>(end - begin));
}

/// The zero-copy path: a text cell straight out of the batch's own buffer.
QString stringFromColumn(const ReldexColumnView &view, std::size_t row)
{
    if (view.data == nullptr || view.offsets == nullptr || row >= view.row_count) {
        return {};
    }
    const std::size_t begin = view.offsets[row];
    const std::size_t end = view.offsets[row + 1];
    if (end < begin || end > view.data_len) {
        return {};
    }
    return QString::fromUtf8(reinterpret_cast<const char *>(view.data + begin),
                             static_cast<qsizetype>(end - begin));
}

} // namespace

ResultTableModel::ResultTableModel(QObject *parent)
    : QAbstractTableModel(parent)
{
    rebuildFormatOptions();
}

ResultTableModel::~ResultTableModel()
{
    // Every batch must be released before the hub is destroyed. That holds
    // because Bridge deletes its SessionController -- and this model with it
    // -- before it calls reldex_hub_destroy.
    releaseEverything();
}

void ResultTableModel::setFetchSource(ResultFetchSource *source)
{
    m_source = source;
}

int ResultTableModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rowCount;
}

int ResultTableModel::columnCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_columnCount;
}

QHash<int, QByteArray> ResultTableModel::roleNames() const
{
    // Deliberately minimal: what a plain `Text` delegate binds to, plus the
    // NULL flag `SPEC.md` §24 needs (NULL, empty and taken are three states).
    return {
        { Qt::DisplayRole, QByteArrayLiteral("display") },
        { IsNullRole, QByteArrayLiteral("isNull") },
    };
}

QVariant ResultTableModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.parent().isValid()) {
        return {};
    }
    if (role != Qt::DisplayRole && role != IsNullRole) {
        return {};
    }
    const int row = index.row();
    const int column = index.column();
    if (row < 0 || row >= m_rowCount || column < 0 || column >= m_columnCount) {
        return {};
    }

    const int batchIndex = batchForRow(row);
    if (batchIndex < 0) {
        return {};
    }
    BatchEntry &batch = m_batches[static_cast<std::size_t>(batchIndex)];
    const auto localRow = static_cast<std::size_t>(row - batch.firstRow);
    if (static_cast<std::size_t>(column) >= batch.views.size()) {
        return {};
    }
    const ReldexColumnView &view = batch.views[static_cast<std::size_t>(column)];

    // A pointer read of the batch's own NULL bitmap. No FFI call.
    const bool isNull = reldex::isNullAt(view, localRow);
    if (role == IsNullRole) {
        return isNull;
    }
    if (isNull) {
        return m_nullText;
    }
    if (reldex::isDirectUtf8(view.kind)) {
        // ADR-0003 D4: text is zero-copy. Pointer arithmetic plus one
        // QString::fromUtf8, and nothing else.
        return stringFromColumn(view, localRow);
    }
    // NUMBER, TIMESTAMP and every other kind: the bulk formatter's output,
    // rendered once per (batch, column) and read here as pointer arithmetic.
    return formattedCell(batchIndex, column, static_cast<int>(localRow));
}

QVariant ResultTableModel::headerData(int section, Qt::Orientation orientation, int role) const
{
    if (role != Qt::DisplayRole) {
        return {};
    }
    if (orientation == Qt::Vertical) {
        return section >= 0 && section < m_rowCount ? QVariant(section + 1) : QVariant();
    }
    if (section < 0 || section >= m_columnCount) {
        return {};
    }
    if (section < m_columns.size()) {
        return m_columns.at(section).name;
    }
    // The column *count* comes from the EXECUTED event and the *names* from
    // the first batch (ADR-0003 A6), so there is a window where one is known
    // and the other is not.
    return QString::number(section + 1);
}

bool ResultTableModel::canFetchMore(const QModelIndex &parent) const
{
    if (parent.isValid() || m_source == nullptr || m_rowLimitReached) {
        return false;
    }
    return m_source->canFetchMoreRows();
}

void ResultTableModel::fetchMore(const QModelIndex &parent)
{
    if (parent.isValid() || m_source == nullptr || m_rowLimitReached) {
        return;
    }
    m_source->fetchMoreRows();
}

void ResultTableModel::beginResult(int columns)
{
    beginResetModel();
    releaseEverything();
    m_columnCount = std::max(0, columns);
    endResetModel();
    Q_EMIT batchCountChanged();
    Q_EMIT columnNamesChanged();
}

void ResultTableModel::reset()
{
    beginResetModel();
    releaseEverything();
    m_columnCount = 0;
    endResetModel();
    Q_EMIT batchCountChanged();
    Q_EMIT columnNamesChanged();
}

void ResultTableModel::applyBatch(reldex::BatchHandle batch, int rows)
{
    if (!batch) {
        return;
    }
    const bool namesWereUnknown = m_columns.isEmpty();
    if (namesWereUnknown) {
        // The column *count* came from the EXECUTED event; the batch is where
        // the names are (ADR-0003 A6). If the two ever disagreed, changing
        // columnCount() outside a reset would be a model-consistency bug, so
        // that case takes a reset -- which is free here, because names are
        // only unknown before the result's first row.
        const auto columns = static_cast<int>(reldex_batch_column_count(batch.get()));
        if (columns > 0 && columns != m_columnCount) {
            beginResetModel();
            releaseEverything();
            m_columnCount = columns;
            readColumnHeaders(batch.get());
            endResetModel();
        } else {
            readColumnHeaders(batch.get());
        }
    }
    // Paired with the mutation `readColumnHeaders()` just made, and emitted
    // *before* any row insertion: a view told about new rows first would read
    // the new headers without ever having been told they changed.
    if (namesWereUnknown && m_columnCount > 0) {
        Q_EMIT headerDataChanged(Qt::Horizontal, 0, m_columnCount - 1);
        Q_EMIT columnNamesChanged();
    }

    if (rows <= 0) {
        // An exhausted-result marker: its column names have been taken, and it
        // has no rows to keep, so it is released as this handle goes away.
        return;
    }

    // `QAbstractItemModel` counts rows in `int`, so a stream long enough to
    // overflow one must be refused rather than wrapped. `m_maxRows` is that
    // ceiling, lowered by a test or by a caller that wants a smaller one.
    const qint64 room = static_cast<qint64>(m_maxRows) - static_cast<qint64>(m_rowCount);
    if (room <= 0 || rows > room) {
        const int accepted = static_cast<int>(std::max<qint64>(0, room));
        if (!m_rowLimitReached) {
            m_rowLimitReached = true;
            qWarning("ResultTableModel: row limit %d reached; %d further row(s) in this batch and "
                     "every later batch are refused. What was already fetched stays readable.",
                     m_maxRows, rows - accepted);
            Q_EMIT rowLimitReachedChanged();
        }
        if (accepted == 0) {
            return; // the handle releases the batch
        }
        rows = accepted;
    }

    BatchEntry entry;
    entry.firstRow = m_rowCount;
    entry.rows = rows;
    entry.views.resize(static_cast<std::size_t>(m_columnCount));
    for (int column = 0; column < m_columnCount; ++column) {
        // Once per column per batch (ADR-0003 D4), never per cell.
        ReldexColumnView view = reldex::makeColumnView();
        if (reldex_batch_column(batch.get(), static_cast<std::size_t>(column), &view)
            != RELDEX_STATUS_OK) {
            view = reldex::makeColumnView();
        }
        entry.views[static_cast<std::size_t>(column)] = view;
    }
    entry.handle = std::move(batch);

    beginInsertRows(QModelIndex(), m_rowCount, m_rowCount + rows - 1);
    m_firstRows.push_back(entry.firstRow);
    m_batches.push_back(std::move(entry));
    m_rowCount += rows;
    endInsertRows();

    Q_EMIT batchCountChanged();
}

QStringList ResultTableModel::columnNames() const
{
    QStringList names;
    names.reserve(m_columns.size());
    for (const ColumnHeader &column : m_columns) {
        names.append(column.name);
    }
    return names;
}

void ResultTableModel::readColumnHeaders(const ReldexBatch *batch)
{
    const std::size_t columns = reldex_batch_column_count(batch);
    if (columns == 0) {
        return;
    }
    m_columns.clear();
    m_columns.reserve(static_cast<qsizetype>(columns));
    for (std::size_t column = 0; column < columns; ++column) {
        ReldexColumnInfo info = reldex::makeColumnInfo();
        ColumnHeader header;
        if (reldex_batch_column_info(batch, column, &info) == RELDEX_STATUS_OK) {
            // A13: every string out of Reldex is NUL-terminated, but `len` is
            // authoritative, so the length is always passed.
            header.name = QString::fromUtf8(reinterpret_cast<const char *>(info.name.ptr),
                                            static_cast<qsizetype>(info.name.len));
            header.kind = info.kind;
        }
        m_columns.append(header);
    }
}

int ResultTableModel::batchForRow(int row) const
{
    if (m_batches.empty()) {
        return -1;
    }
    if (m_lastBatch >= 0 && m_lastBatch < static_cast<int>(m_batches.size())) {
        const BatchEntry &cached = m_batches[static_cast<std::size_t>(m_lastBatch)];
        if (row >= cached.firstRow && row < cached.firstRow + cached.rows) {
            return m_lastBatch;
        }
    }
    const auto upper = std::upper_bound(m_firstRows.begin(), m_firstRows.end(), row);
    const auto index = static_cast<int>(upper - m_firstRows.begin()) - 1;
    if (index < 0) {
        return -1;
    }
    m_lastBatch = index;
    return index;
}

QString ResultTableModel::formattedCell(int batchIndex, int column, int localRow) const
{
    const BatchEntry &batch = m_batches[static_cast<std::size_t>(batchIndex)];
    const WindowKey key { batchIndex, column, localRow / kWindowRows };

    const FormattedWindow *entry = nullptr;
    const auto cached = m_windows.find(key);
    if (cached != m_windows.end()) {
        touch(cached->second);
        entry = &cached->second->second;
    } else {
        entry = hydrate(key, batch.rows, batch.handle.get());
    }
    if (entry == nullptr || entry->failed) {
        return {};
    }
    // A deep copy out of the arena, so a later eviction cannot dangle behind a
    // QString the view already handed to QML.
    return stringFromArena(entry->view, static_cast<std::size_t>(localRow - entry->firstLocalRow));
}

const ResultTableModel::FormattedWindow *
ResultTableModel::hydrate(const WindowKey &key, int rowsInBatch, const ReldexBatch *batch) const
{
    // THE ONE FFI PATH REACHED FROM data(): ADR-0003 D4's bulk formatter, over
    // one bounded window -- "one call per visible window, not per cell", and
    // not per *batch* either, because a batch is whatever `fetchRows` says and
    // a big fetch would otherwise put a whole batch's formatting cost inside
    // the first cell of it.
    const int firstLocalRow = key.window * kWindowRows;
    const int rows = std::min(kWindowRows, rowsInBatch - firstLocalRow);
    if (rows <= 0) {
        return nullptr; // not cached: there is nothing to render or to remember
    }

    FormattedWindow rendered;
    rendered.firstLocalRow = firstLocalRow;

    reldex::ArenaHandle arena(reldex_text_arena_create());
    if (!arena) {
        rendered.failed = true;
        noteFormatFailure("reldex_text_arena_create");
    } else if (reldex_batch_format_column(batch, static_cast<std::size_t>(key.column),
                                          static_cast<std::size_t>(firstLocalRow),
                                          static_cast<std::size_t>(rows), &m_formatOptions,
                                          arena.get())
               != RELDEX_STATUS_OK) {
        rendered.failed = true;
        noteFormatFailure("reldex_batch_format_column");
    } else {
        ReldexArenaView view = reldex::makeArenaView();
        if (reldex_text_arena_view(arena.get(), &view) != RELDEX_STATUS_OK) {
            rendered.failed = true;
            noteFormatFailure("reldex_text_arena_view");
        } else {
            rendered.view = view;
            rendered.bytes = static_cast<qint64>(view.data_len)
                    + static_cast<qint64>((view.count + 1) * sizeof(std::size_t));
            rendered.arena = std::move(arena);
        }
    }

    // A failed window is cached too -- with no arena and no bytes -- because
    // that is what stops `data()` from retrying, and therefore calling FFI, on
    // every repaint of every cell in it.
    //
    // Inserted first and evicted afterwards: reserving an LRU slot before the
    // render succeeded would evict a live window to make room for one that may
    // never exist.
    m_lru.emplace_front(key, std::move(rendered));
    m_windows.emplace(key, m_lru.begin());
    m_formattedBytes += m_lru.front().second.bytes;

    evictIfNeeded(key);
    return &m_lru.front().second;
}

void ResultTableModel::touch(WindowList::iterator node) const
{
    // Moves the node, not its contents: every iterator the index holds --
    // including this one -- stays valid.
    m_lru.splice(m_lru.begin(), m_lru, node);
}

void ResultTableModel::evictIfNeeded(const WindowKey &keep) const
{
    const int windowLimit = std::max(1, m_maxFormattedWindows);
    while (!m_lru.empty()
           && (static_cast<int>(m_lru.size()) > windowLimit
               || m_formattedBytes > m_maxFormattedBytes)) {
        const auto victim = std::prev(m_lru.end());
        if (victim->first == keep) {
            // The window being read right now is last in the LRU, which means
            // it is the only one left and is over the byte bound on its own.
            // Keeping it is the lesser evil: dropping it would make the very
            // next cell re-render it, which is the per-cell FFI this design
            // exists to avoid. The bound is a target, not a guarantee, and it
            // can be exceeded by at most one window.
            break;
        }
        m_formattedBytes -= victim->second.bytes;
        m_windows.erase(victim->first);
        m_lru.erase(victim);
    }
}

void ResultTableModel::dropAllFormattedText() const
{
    m_lru.clear();
    m_windows.clear();
    m_formattedBytes = 0;
}

void ResultTableModel::noteFormatFailure(const char *what) const
{
    if (m_formattingFailed) {
        return; // said once, not once per repaint
    }
    m_formattingFailed = true;
    qWarning("ResultTableModel: %s failed; affected cells render empty. This is reported once.",
             what);
    Q_EMIT const_cast<ResultTableModel *>(this)->formattingFailedChanged();
}

void ResultTableModel::releaseEverything()
{
    // Arenas first: a window borrows nothing from its batch once rendered, but
    // the keys index batches by position, so nothing may outlive `m_batches`.
    dropAllFormattedText();
    m_batches.clear(); // releases every batch
    m_firstRows.clear();
    m_columns.clear();
    m_rowCount = 0;
    m_lastBatch = -1;
    if (m_rowLimitReached) {
        m_rowLimitReached = false;
        Q_EMIT rowLimitReachedChanged();
    }
}

void ResultTableModel::setMaxFormattedWindows(int windows)
{
    const int clamped = std::max(1, windows);
    if (clamped == m_maxFormattedWindows) {
        return;
    }
    m_maxFormattedWindows = clamped;
    evictIfNeeded(WindowKey { -1, -1, -1 }); // no window is being read here
    Q_EMIT formattedBoundChanged();
}

void ResultTableModel::setMaxFormattedBytes(qint64 bytes)
{
    const qint64 clamped = std::max<qint64>(0, bytes);
    if (clamped == m_maxFormattedBytes) {
        return;
    }
    m_maxFormattedBytes = clamped;
    evictIfNeeded(WindowKey { -1, -1, -1 });
    Q_EMIT formattedBoundChanged();
}

void ResultTableModel::setMaxRows(int rows)
{
    const int clamped = std::max(0, rows);
    if (clamped == m_maxRows) {
        return;
    }
    m_maxRows = clamped;
    Q_EMIT maxRowsChanged();
}

void ResultTableModel::setNullText(const QString &text)
{
    if (text == m_nullText) {
        return;
    }
    m_nullText = text;
    reformatEverything();
    Q_EMIT nullTextChanged();
}

void ResultTableModel::setTimestampStyle(int style)
{
    if (style == m_timestampStyle) {
        return;
    }
    m_timestampStyle = style;
    reformatEverything();
    Q_EMIT timestampStyleChanged();
}

void ResultTableModel::reformatEverything()
{
    rebuildFormatOptions();
    dropAllFormattedText();
    if (m_rowCount > 0 && m_columnCount > 0) {
        Q_EMIT dataChanged(index(0, 0), index(m_rowCount - 1, m_columnCount - 1),
                           { Qt::DisplayRole });
    }
}

void ResultTableModel::rebuildFormatOptions()
{
    m_nullTextUtf8 = m_nullText.toUtf8();
    m_formatOptions = reldex::makeFormatOptions();
    m_formatOptions.timestamp_style = m_timestampStyle;
    m_formatOptions.null_text.ptr =
            reinterpret_cast<const std::uint8_t *>(m_nullTextUtf8.constData());
    m_formatOptions.null_text.len = static_cast<std::size_t>(m_nullTextUtf8.size());
}
