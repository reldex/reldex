#include "ProfileModel.h"

#include <algorithm>

namespace {

QByteArray idKey(const QByteArray &id)
{
    // 16 raw bytes compare exactly; no normalization needed on this side --
    // the workspace's own ids are already canonical (ADR-0006 P3, P5's
    // COLLATE NOCASE is a store-text concern, not this in-memory model's).
    return id;
}

} // namespace

ProfileModel::ProfileModel(QObject *parent) : QAbstractListModel(parent) { }

int ProfileModel::rowCount(const QModelIndex &parent) const
{
    if (parent.isValid()) {
        return 0;
    }
    return m_rows.size();
}

QVariant ProfileModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const Row &row = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return QString::fromLatin1(row.id.toHex());
    case NameRole:
        return row.name;
    case EnvironmentRole:
        return row.environment;
    case EnvironmentLabelRole:
        return row.environmentLabel;
    case TreatAsProductionRole:
        return row.treatAsProduction;
    case EndpointSummaryRole:
        return row.endpointSummary;
    case UsernameRole:
        return row.username;
    case PasswordStorageRole:
        return row.passwordStorage;
    case DatabaseTypeRole:
        return row.databaseType;
    case SessionRoleRole:
        return row.sessionRole;
    case TransportRole:
        return row.transport;
    default:
        return {};
    }
}

QHash<int, QByteArray> ProfileModel::roleNames() const
{
    return {
        { IdRole, "profileId" },
        { NameRole, "name" },
        { EnvironmentRole, "environment" },
        { EnvironmentLabelRole, "environmentLabel" },
        { TreatAsProductionRole, "treatAsProduction" },
        { EndpointSummaryRole, "endpointSummary" },
        { UsernameRole, "username" },
        { PasswordStorageRole, "passwordStorage" },
        { DatabaseTypeRole, "databaseType" },
        { SessionRoleRole, "sessionRole" },
        { TransportRole, "transport" },
    };
}

QVariantMap ProfileModel::get(int row) const
{
    if (row < 0 || row >= m_rows.size()) {
        return {};
    }
    const Row &entry = m_rows.at(row);
    return QVariantMap {
        { QStringLiteral("id"), QString::fromLatin1(entry.id.toHex()) },
        { QStringLiteral("name"), entry.name },
        { QStringLiteral("environment"), entry.environment },
        { QStringLiteral("environmentLabel"), entry.environmentLabel },
        { QStringLiteral("treatAsProduction"), entry.treatAsProduction },
        { QStringLiteral("endpointKind"), entry.endpointKind },
        { QStringLiteral("host"), entry.host },
        { QStringLiteral("port"), entry.port },
        { QStringLiteral("serviceTargetKind"), entry.serviceTargetKind },
        { QStringLiteral("serviceNameOrSid"), entry.serviceNameOrSid },
        { QStringLiteral("connectString"), entry.connectString },
        { QStringLiteral("username"), entry.username },
        { QStringLiteral("authKind"), entry.authKind },
        { QStringLiteral("passwordStorage"), entry.passwordStorage },
        { QStringLiteral("databaseType"), entry.databaseType },
        { QStringLiteral("sessionRole"), entry.sessionRole },
        { QStringLiteral("transport"), entry.transport },
        { QStringLiteral("caDirectory"), entry.caDirectory },
        { QStringLiteral("allowUnenforcedCertificatePin"), entry.allowUnenforcedCertificatePin },
    };
}

int ProfileModel::indexOfId(const QString &idHex) const
{
    const QByteArray id = QByteArray::fromHex(idHex.toLatin1());
    for (int i = 0; i < m_rows.size(); ++i) {
        if (m_rows.at(i).id == id) {
            return i;
        }
    }
    return -1;
}

int ProfileModel::sortedInsertPosition(const QString &name) const
{
    // `reldex_workspace_list_profiles` documents "every profile, by name"
    // (case-sensitive Rust `str` ordering); mirrored here with a plain
    // locale-independent compare so a locally upserted row lands where a
    // fresh `list_profiles` reply would put it, and the list never visibly
    // reorders itself around an edit.
    auto it = std::lower_bound(m_rows.cbegin(), m_rows.cend(), name,
                                [](const Row &row, const QString &value) {
                                    return QString::compare(row.name, value, Qt::CaseSensitive) < 0;
                                });
    return static_cast<int>(it - m_rows.cbegin());
}

void ProfileModel::resetRows(QVector<Row> rows)
{
    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
}

void ProfileModel::upsertRow(const Row &row)
{
    const QByteArray key = idKey(row.id);
    for (int i = 0; i < m_rows.size(); ++i) {
        if (idKey(m_rows.at(i).id) == key) {
            if (m_rows.at(i).name == row.name) {
                m_rows[i] = row;
                const QModelIndex changed = index(i);
                Q_EMIT dataChanged(changed, changed);
                return;
            }
            // The name changed, so the sorted position might too: remove and
            // reinsert rather than resort the whole list for one row.
            beginRemoveRows({}, i, i);
            m_rows.removeAt(i);
            endRemoveRows();
            break;
        }
    }
    const int position = sortedInsertPosition(row.name);
    beginInsertRows({}, position, position);
    m_rows.insert(position, row);
    endInsertRows();
}

void ProfileModel::removeRow(const QByteArray &id)
{
    const QByteArray key = idKey(id);
    for (int i = 0; i < m_rows.size(); ++i) {
        if (idKey(m_rows.at(i).id) == key) {
            beginRemoveRows({}, i, i);
            m_rows.removeAt(i);
            endRemoveRows();
            return;
        }
    }
}

const ProfileModel::Row *ProfileModel::findRow(const QByteArray &id) const
{
    const QByteArray key = idKey(id);
    for (const Row &row : m_rows) {
        if (idKey(row.id) == key) {
            return &row;
        }
    }
    return nullptr;
}
