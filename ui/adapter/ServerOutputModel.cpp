#include "ServerOutputModel.h"

ServerOutputModel::ServerOutputModel(QObject *parent) : QAbstractListModel(parent) { }

int ServerOutputModel::rowCount(const QModelIndex &parent) const
{
    if (parent.isValid()) {
        return 0;
    }
    return static_cast<int>(m_lines.size());
}

QVariant ServerOutputModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0
        || static_cast<std::size_t>(index.row()) >= m_lines.size()) {
        return {};
    }
    if (role == Qt::DisplayRole || role == LineTextRole) {
        return m_lines[static_cast<std::size_t>(index.row())];
    }
    return {};
}

QHash<int, QByteArray> ServerOutputModel::roleNames() const
{
    return {
        { Qt::DisplayRole, QByteArrayLiteral("display") },
        { LineTextRole, QByteArrayLiteral("lineText") },
    };
}

void ServerOutputModel::appendLines(const QStringList &lines)
{
    if (lines.isEmpty()) {
        return;
    }
    const int first = static_cast<int>(m_lines.size());
    const int last = first + static_cast<int>(lines.size()) - 1;
    beginInsertRows(QModelIndex(), first, last);
    m_lines.reserve(m_lines.size() + static_cast<std::size_t>(lines.size()));
    for (const QString &line : lines) {
        m_lines.push_back(line);
    }
    endInsertRows();
}

void ServerOutputModel::clear()
{
    if (m_lines.empty()) {
        return;
    }
    beginResetModel();
    m_lines.clear();
    m_lines.shrink_to_fit();
    endResetModel();
}
