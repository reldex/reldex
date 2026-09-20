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
    if (parent.isValid() || m_source == nullptr) {
        return false;
    }
    return m_source->canFetchMoreRows();
}

void ResultTableModel::fetchMore(const QModelIndex &parent)
{
    if (parent.isValid() || m_source == nullptr) {
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
    if (rows <= 0) {
        // An exhausted-result marker: its column names have been taken, and it
        // has no rows to keep, so it is released as this handle goes away.
        if (namesWereUnknown && m_columnCount > 0) {
            Q_EMIT headerDataChanged(Qt::Horizontal, 0, m_columnCount - 1);
            Q_EMIT columnNamesChanged();
        }
        return;
    }

    BatchEntry entry;
    entry.firstRow = m_rowCount;
    entry.rows = rows;
    entry.views.resize(static_cast<std::size_t>(m_columnCount));
    entry.formatted.resize(static_cast<std::size_t>(m_columnCount));
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

    if (namesWereUnknown && m_columnCount > 0) {
        Q_EMIT headerDataChanged(Qt::Horizontal, 0, m_columnCount - 1);
        Q_EMIT columnNamesChanged();
    }
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
    BatchEntry &batch = m_batches[static_cast<std::size_t>(batchIndex)];
    FormattedColumn &formatted = batch.formatted[static_cast<std::size_t>(column)];
    if (!formatted.arena) {
        hydrate(batchIndex, column);
    }
    // `hydrate()` can evict other batches but never this one: `touch()` moves
    // it to the most-recently-used end before anything is dropped.
    return stringFromArena(formatted.view, static_cast<std::size_t>(localRow));
}

void ResultTableModel::hydrate(int batchIndex, int column) const
{
    // THE ONE FFI PATH REACHED FROM data(): ADR-0003 D4's bulk formatter, run
    // once per (batch, column) while that batch keeps its cache -- "one call
    // per visible window, not per cell".
    BatchEntry &batch = m_batches[static_cast<std::size_t>(batchIndex)];
    FormattedColumn &formatted = batch.formatted[static_cast<std::size_t>(column)];

    touch(batchIndex);

    reldex::ArenaHandle arena(reldex_text_arena_create());
    if (!arena) {
        return;
    }
    if (reldex_batch_format_column(batch.handle.get(), static_cast<std::size_t>(column), 0,
                                   static_cast<std::size_t>(batch.rows), &m_formatOptions,
                                   arena.get())
        != RELDEX_STATUS_OK) {
        return;
    }
    ReldexArenaView view = reldex::makeArenaView();
    if (reldex_text_arena_view(arena.get(), &view) != RELDEX_STATUS_OK) {
        return;
    }
    formatted.arena = std::move(arena);
    formatted.view = view;

    evictIfNeeded();
}

void ResultTableModel::touch(int batchIndex) const
{
    const auto existing = std::find(m_hydrationOrder.begin(), m_hydrationOrder.end(), batchIndex);
    if (existing != m_hydrationOrder.end()) {
        m_hydrationOrder.erase(existing);
    }
    m_hydrationOrder.push_back(batchIndex);
}

void ResultTableModel::dropFormattedText(int batchIndex) const
{
    if (batchIndex < 0 || batchIndex >= static_cast<int>(m_batches.size())) {
        return;
    }
    BatchEntry &batch = m_batches[static_cast<std::size_t>(batchIndex)];
    for (FormattedColumn &formatted : batch.formatted) {
        formatted.arena.reset();
        formatted.view = ReldexArenaView {};
    }
}

void ResultTableModel::dropAllFormattedText() const
{
    for (int index = 0; index < static_cast<int>(m_batches.size()); ++index) {
        dropFormattedText(index);
    }
    m_hydrationOrder.clear();
}

void ResultTableModel::evictIfNeeded() const
{
    const int limit = std::max(1, m_maxCachedBatches);
    while (static_cast<int>(m_hydrationOrder.size()) > limit) {
        const int victim = m_hydrationOrder.front();
        m_hydrationOrder.erase(m_hydrationOrder.begin());
        dropFormattedText(victim);
    }
}

int ResultTableModel::hydratedColumnCount() const
{
    int count = 0;
    for (const BatchEntry &batch : m_batches) {
        for (const FormattedColumn &formatted : batch.formatted) {
            if (formatted.arena) {
                ++count;
            }
        }
    }
    return count;
}

void ResultTableModel::releaseEverything()
{
    m_hydrationOrder.clear();
    m_batches.clear(); // releases every arena, then every batch
    m_firstRows.clear();
    m_columns.clear();
    m_rowCount = 0;
    m_lastBatch = -1;
}

void ResultTableModel::setMaxCachedBatches(int batches)
{
    const int clamped = std::max(1, batches);
    if (clamped == m_maxCachedBatches) {
        return;
    }
    m_maxCachedBatches = clamped;
    evictIfNeeded();
    Q_EMIT maxCachedBatchesChanged();
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
