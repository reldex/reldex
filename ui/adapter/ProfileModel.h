#pragma once

// `ProfileModel` -- M3.2: a read-only `QAbstractListModel` projection of the
// workspace's stored connection profiles (`SPEC.md` §17, ADR-0006 P3).
//
// Deliberately thin (`AGENTS.md`): it holds exactly what a list/edit dialog
// needs to display and never a password -- there is no role, property or
// method here that could expose one, by construction (`ConnectionManager`
// never hands this class a `ReldexSecret` or a plaintext password). Every
// mutation comes from `ConnectionManager`, which is the only class that talks
// to the workspace's service thread; this model is not `QML_UNCREATABLE`'s
// friend list, it is simply never constructed anywhere else.

#include <QAbstractListModel>
#include <QByteArray>
#include <QString>
#include <QVariantMap>
#include <QVector>
#include <QtQml/qqmlregistration.h>

class ProfileModel : public QAbstractListModel
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ProfileModel is created by ConnectionManager and reached as "
                     "bridge.connections.profiles")

public:
    enum Role {
        IdRole = Qt::UserRole + 1,
        NameRole,
        EnvironmentRole,
        EnvironmentLabelRole,
        TreatAsProductionRole,
        EndpointSummaryRole,
        UsernameRole,
        PasswordStorageRole,
        DatabaseTypeRole,
        SessionRoleRole,
        TransportRole,
    };
    Q_ENUM(Role)

    /// Everything the list (and the edit dialog's pre-fill) needs. Field
    /// names mirror `ReldexProfileView`'s, minus everything password-shaped.
    struct Row
    {
        QByteArray id; // 16 raw bytes
        QString name;
        int environment = 0; // ReldexEnvironmentKind
        QString environmentLabel;
        bool treatAsProduction = false;
        QString endpointSummary; // built once, from the view -- never a secret
        int endpointKind = 0; // ReldexEndpointKind
        QString host;
        int port = 0;
        int serviceTargetKind = 0; // ReldexServiceTargetKind
        QString serviceNameOrSid;
        QString connectString;
        QString username;
        int authKind = 0; // ReldexAuthKind
        int passwordStorage = 0; // ReldexPasswordStorageKind
        int databaseType = 0; // ReldexDatabaseType
        int sessionRole = 0; // ReldexSessionRoleKind
        int transport = 0; // ReldexTransportKind
        QString caDirectory;
        bool allowUnenforcedCertificatePin = false;
    };

    explicit ProfileModel(QObject *parent = nullptr);

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role) const override;
    [[nodiscard]] QHash<int, QByteArray> roleNames() const override;

    /// Every field of one row, as a `QVariantMap` an edit dialog can bind
    /// against directly -- keys match `ConnectionManager::createProfile()`'s
    /// input map, so "load into the form" and "save the form" use the same
    /// shape.
    Q_INVOKABLE [[nodiscard]] QVariantMap get(int row) const;
    /// The row for `idHex` (lowercase hyphenated UUID text), or -1.
    Q_INVOKABLE [[nodiscard]] int indexOfId(const QString &idHex) const;

    // --- mutation: ConnectionManager only -----------------------------------
    void resetRows(QVector<Row> rows);
    /// Inserts, or replaces in place, keeping the list sorted by name (the
    /// same order `reldex_workspace_list_profiles` returns).
    void upsertRow(const Row &row);
    void removeRow(const QByteArray &id);
    [[nodiscard]] const Row *findRow(const QByteArray &id) const;

private:
    [[nodiscard]] int sortedInsertPosition(const QString &name) const;

    QVector<Row> m_rows;
};
