#include "ObjectBrowserModel.h"

#include "Bridge.h"
#include "SessionController.h"

#include <QByteArray>
#include <QCoreApplication>
#include <QVariantMap>

#include <algorithm>
#include <array>

namespace {

struct GroupSpec
{
    int kind;
    const char *label;
};

/// The 9 object groups `SPEC.md` §16 lists besides `Schemas` (which is the
/// `Connection` node's own fetch, not a group). Order is display order.
constexpr std::array<GroupSpec, 9> kGroups {
    { { RELDEX_METADATA_OBJECT_KIND_TABLES, QT_TR_NOOP("Tables") },
      { RELDEX_METADATA_OBJECT_KIND_VIEWS, QT_TR_NOOP("Views") },
      { RELDEX_METADATA_OBJECT_KIND_PACKAGES, QT_TR_NOOP("Packages") },
      { RELDEX_METADATA_OBJECT_KIND_PACKAGE_BODIES, QT_TR_NOOP("Package Bodies") },
      { RELDEX_METADATA_OBJECT_KIND_PROCEDURES, QT_TR_NOOP("Procedures") },
      { RELDEX_METADATA_OBJECT_KIND_FUNCTIONS, QT_TR_NOOP("Functions") },
      { RELDEX_METADATA_OBJECT_KIND_TRIGGERS, QT_TR_NOOP("Triggers") },
      { RELDEX_METADATA_OBJECT_KIND_SEQUENCES, QT_TR_NOOP("Sequences") },
      { RELDEX_METADATA_OBJECT_KIND_SYNONYMS, QT_TR_NOOP("Synonyms") } }
};

[[nodiscard]] ReldexStr toReldexStr(const QByteArray &utf8) noexcept
{
    return ReldexStr { reinterpret_cast<const std::uint8_t *>(utf8.constData()),
                       static_cast<std::size_t>(utf8.size()) };
}

[[nodiscard]] QString fromReldexStr(const ReldexStr &text)
{
    if (text.ptr == nullptr || text.len == 0) {
        return {};
    }
    return QString::fromUtf8(reinterpret_cast<const char *>(text.ptr),
                             static_cast<qsizetype>(text.len));
}

} // namespace

ObjectBrowserModel::ObjectBrowserModel(QObject *parent) : QAbstractItemModel(parent)
{
    m_root = std::make_unique<Node>();
    m_root->kind = NodeKind::Root;
}

ObjectBrowserModel::~ObjectBrowserModel()
{
    // Fire-and-forget, like every other close in this adapter (Bridge's own
    // teardown doc comment): this must not block the caller, and a session
    // being destroyed alongside its owning QObject tree needs no reply.
    closeConnection();
}

ObjectBrowserModel::Node *ObjectBrowserModel::nodeFor(const QModelIndex &index) const
{
    if (!index.isValid()) {
        return m_root.get();
    }
    return static_cast<Node *>(index.internalPointer());
}

QModelIndex ObjectBrowserModel::indexFor(Node *node, int column) const
{
    if (node == nullptr || node->parent == nullptr) {
        return {};
    }
    const auto &siblings = node->parent->children;
    for (std::size_t row = 0; row < siblings.size(); ++row) {
        if (siblings[row].get() == node) {
            return createIndex(static_cast<int>(row), column, node);
        }
    }
    return {};
}

QModelIndex ObjectBrowserModel::index(int row, int column, const QModelIndex &parent) const
{
    if (row < 0 || column != 0) {
        return {};
    }
    Node *parentNode = nodeFor(parent);
    if (parentNode == nullptr || row >= static_cast<int>(parentNode->children.size())) {
        return {};
    }
    return createIndex(row, column, parentNode->children[static_cast<std::size_t>(row)].get());
}

QModelIndex ObjectBrowserModel::parent(const QModelIndex &child) const
{
    if (!child.isValid()) {
        return {};
    }
    Node *node = nodeFor(child);
    if (node == nullptr || node->parent == nullptr || node->parent == m_root.get()) {
        return {};
    }
    return indexFor(node->parent);
}

int ObjectBrowserModel::rowCount(const QModelIndex &parent) const
{
    if (parent.column() > 0) {
        return 0;
    }
    Node *node = nodeFor(parent);
    return node != nullptr ? static_cast<int>(node->children.size()) : 0;
}

int ObjectBrowserModel::columnCount(const QModelIndex &parent) const
{
    Q_UNUSED(parent);
    return 1;
}

bool ObjectBrowserModel::hasChildren(const QModelIndex &parent) const
{
    Node *node = nodeFor(parent);
    if (node == nullptr) {
        return false;
    }
    switch (node->kind) {
    case NodeKind::Root:
    case NodeKind::Connection:
    case NodeKind::Schema:
    case NodeKind::Group:
    case NodeKind::ColumnsNode:
        // Always expandable, even before the first load -- this is what
        // makes lazy loading possible: a view shows the expand affordance
        // without this model having fetched anything yet.
        return true;
    case NodeKind::Object:
        return node->isTableLike;
    case NodeKind::Column:
        return false;
    }
    return false;
}

QVariant ObjectBrowserModel::data(const QModelIndex &index, int role) const
{
    Node *node = nodeFor(index);
    if (node == nullptr || node == m_root.get()) {
        return {};
    }
    switch (role) {
    case Qt::DisplayRole:
    case NameRole:
        return node->name;
    case SecondaryRole:
        return node->secondary;
    case KindRole:
        return QVariant::fromValue(static_cast<int>(node->kind));
    case LoadingRole:
        return node->loading;
    case HasErrorRole:
        return node->hasError;
    case ErrorTextRole:
        return node->errorText;
    case TruncatedRole:
        return node->truncated;
    case StatusTextRole:
        if (node->loading) {
            return tr("Loading…");
        }
        if (node->hasError) {
            return node->errorText;
        }
        if (node->shownCount < 0) {
            return QString();
        }
        if (node->truncated) {
            return tr("%1 shown, more available").arg(node->shownCount);
        }
        if (node->kind == NodeKind::Connection || node->kind == NodeKind::Group) {
            return tr("%1 shown").arg(node->shownCount);
        }
        return QString();
    default:
        return {};
    }
}

QHash<int, QByteArray> ObjectBrowserModel::roleNames() const
{
    return {
        // `Qt::DisplayRole` must be exposed under "display" explicitly:
        // `TreeViewDelegate`'s own default implementation reads
        // `model.display`, and a QML delegate's `model.<name>` lookup only
        // resolves a name this hash declares -- it is not implied just
        // because `data()` answers `Qt::DisplayRole` from C++. Without this
        // entry, `TreeViewDelegate.qml`'s own internal binding gets
        // `undefined` and Qt Quick logs "Unable to assign [undefined] to
        // QString" as a QML warning (which fails `tst_coreinfo`'s
        // no-warnings assertion for any surface that loads this model).
        { Qt::DisplayRole, "display" },
        { NameRole, "name" },       { SecondaryRole, "secondary" }, { KindRole, "kind" },
        { LoadingRole, "loading" }, { HasErrorRole, "hasError" },   { ErrorTextRole, "errorText" },
        { TruncatedRole, "truncated" }, { StatusTextRole, "statusText" },
    };
}

void ObjectBrowserModel::setBridge(Bridge *bridge)
{
    if (m_bridge == bridge) {
        return;
    }
    closeConnection();
    m_bridge = bridge;

    beginResetModel();
    m_root = std::make_unique<Node>();
    m_root->kind = NodeKind::Root;
    m_pendingQueue.clear();
    m_activeNode = nullptr;
    m_activeQuery.reset();
    m_nodeGeneration.clear();
    ++m_generation;
    m_columnsPaneNode = nullptr;
    endResetModel();

    m_columnsPaneTitle.clear();
    m_columnsPaneRows.clear();
    m_columnsPaneLoading = false;
    m_columnsPaneError.clear();
    Q_EMIT columnsPaneChanged();

    if (m_bridge != nullptr) {
        ensureConnectionRoot();
    }
    Q_EMIT bridgeChanged();
}

void ObjectBrowserModel::setRowCap(int cap)
{
    const int clamped = std::max(1, cap);
    if (clamped == m_rowCap) {
        return;
    }
    m_rowCap = clamped;
    Q_EMIT rowCapChanged();
}

bool ObjectBrowserModel::isSessionOpen() const noexcept
{
    // Deliberately does NOT exclude `SessionController::Failed`: that state
    // also covers "the last statement on this otherwise-healthy session
    // failed", which must not stop the *next* fetch from being attempted
    // (`m_sessionOpenFailed` is the narrower, correct signal for "give up on
    // this session entirely").
    return m_metadataSession != nullptr && m_metadataSession->sessionId() != 0 && !m_sessionOpenFailed
            && m_metadataSession->state() != SessionController::Closed
            && m_metadataSession->state() != SessionController::Closing;
}

bool ObjectBrowserModel::isSessionOpening() const noexcept
{
    return m_metadataSession != nullptr && m_metadataSession->state() == SessionController::Opening;
}

quint64 ObjectBrowserModel::sessionId() const noexcept
{
    return m_metadataSession != nullptr ? m_metadataSession->sessionId() : 0;
}

void ObjectBrowserModel::ensureConnectionRoot()
{
    if (!m_root->children.empty()) {
        return;
    }
    beginInsertRows(QModelIndex(), 0, 0);
    auto connection = std::make_unique<Node>();
    connection->kind = NodeKind::Connection;
    connection->parent = m_root.get();
    connection->name = tr("Connection");
    m_root->children.push_back(std::move(connection));
    endInsertRows();
}

void ObjectBrowserModel::ensureSessionOpen()
{
    if (m_bridge == nullptr || !m_bridge->isValid() || m_metadataSession != nullptr) {
        return;
    }
    // A distinct SessionController, registered against the same Bridge/hub as
    // any worksheet's -- but never the worksheet's own instance (ADR-0002: a
    // worksheet owns its session; the browser owns this one). Opening it
    // pushes the hub's live session count up by one, independent of whatever
    // worksheets are open; closeConnection() brings it back down.
    m_metadataSession = new SessionController(m_bridge, this);
    m_sessionOpenFailed = false;
    m_sessionReady = false;
    connect(m_metadataSession, &SessionController::opened, this,
            &ObjectBrowserModel::onMetadataSessionOpened);
    connect(m_metadataSession, &SessionController::failed, this,
            &ObjectBrowserModel::onMetadataSessionFailed);
    connect(m_metadataSession, &SessionController::resultComplete, this,
            &ObjectBrowserModel::onMetadataResultComplete);
    connect(m_metadataSession, &SessionController::sessionClosed, this,
            &ObjectBrowserModel::onMetadataSessionClosed);
    m_metadataSession->open();
    Q_EMIT sessionOpenChanged();
}

void ObjectBrowserModel::closeConnection()
{
    if (m_metadataSession == nullptr) {
        return;
    }
    // Invalidate every queued/active fetch first, so nothing is left showing
    // "Loading..." forever once the session underneath it is gone.
    ++m_generation;
    const QVector<Node *> queued = m_pendingQueue;
    m_pendingQueue.clear();
    m_activeNode = nullptr;
    m_activeQuery.reset();
    for (Node *node : queued) {
        node->loading = false;
    }

    SessionController *session = m_metadataSession;
    if (session->sessionId() == 0 || session->state() == SessionController::Closed
        || session->state() == SessionController::Closing) {
        m_metadataSession = nullptr;
        session->deleteLater();
        Q_EMIT sessionOpenChanged();
        return;
    }
    if (!m_sessionReady && !m_sessionOpenFailed) {
        // The connect this session started is still outstanding (no OPENED
        // event drained yet): closing now would ask `reldex_session_close`
        // to act on a session it does not yet consider open, which answers
        // `RELDEX_STATUS_INVALID_STATE` -- submitting nothing, and no event
        // ever follows to say so (`m_sessionReady`'s doc comment). Defer:
        // `onMetadataSessionOpened()`/`onMetadataSessionFailed()`'s
        // open-failure branch calls `performClose()` once the connect's
        // outcome is actually known.
        m_closePending = true;
        return;
    }
    performClose();
}

void ObjectBrowserModel::performClose()
{
    SessionController *session = m_metadataSession;
    if (session == nullptr) {
        return;
    }
    m_closePending = false;
    if (session->sessionId() == 0 || session->state() == SessionController::Closed
        || session->state() == SessionController::Closing) {
        m_metadataSession = nullptr;
        session->deleteLater();
        Q_EMIT sessionOpenChanged();
        return;
    }
    m_sessionClosing = true;
    // ROLLBACK, not the library's own `NONE` default: `NONE` comes back
    // `DECISION_REQUIRED` (session left open) whenever the driver cannot rule
    // out an open transaction, which `reldex.h` deliberately never guesses
    // past (SPEC.md §10, "never silently commit"). That decision belongs to
    // a human for a *worksheet* session; this session only ever runs the
    // SELECTs this class itself builds against a dictionary view
    // (`ReldexMetadataRequest` has no write shape), so there is never
    // anything to lose, and always closing with ROLLBACK (never COMMIT) is
    // both safe and the direction that keeps this invisible, backstage
    // session from ever surfacing a "commit or rollback?" prompt no one
    // asked for.
    session->closeSession(RELDEX_CLOSE_DISPOSITION_ROLLBACK);
    // onMetadataSessionClosed() deletes it once SESSION_CLOSED lands -- never
    // blocking here matches every other close in this adapter.
}

void ObjectBrowserModel::onMetadataSessionOpened()
{
    m_sessionReady = true;
    Q_EMIT sessionOpenChanged();
    if (m_closePending) {
        performClose();
        return;
    }
    startNextFetchIfIdle();
}

void ObjectBrowserModel::onMetadataSessionClosed()
{
    m_sessionClosing = false;
    if (m_metadataSession != nullptr) {
        SessionController *session = m_metadataSession;
        m_metadataSession = nullptr;
        session->deleteLater();
    }
    Q_EMIT sessionOpenChanged();
}

bool ObjectBrowserModel::beginChildReset(Node *node)
{
    if (node->children.empty()) {
        return false;
    }
    beginRemoveRows(indexFor(node), 0, static_cast<int>(node->children.size()) - 1);
    node->children.clear();
    return true;
}

void ObjectBrowserModel::endChildReset(bool removalStarted)
{
    if (removalStarted) {
        endRemoveRows();
    }
}

void ObjectBrowserModel::expand(const QModelIndex &index, bool force)
{
    Node *node = nodeFor(index);
    if (node == nullptr || node == m_root.get() || node->kind == NodeKind::Column) {
        return;
    }
    if (!force) {
        // A plain (non-forced) expand on a node that is already loading, or
        // already has an answer, has nothing new to do.
        if (node->loading || node->childrenLoaded) {
            return;
        }
    } else {
        // `force` (a manual refresh, or `setFilter()`, which always forces)
        // supersedes whatever is queued or already in flight for this node --
        // deliberately NOT gated on `node->loading`, unlike the `!force`
        // branch above: this is what makes "the newest request wins" true
        // for a refresh/filter change that arrives before a previous one for
        // the same node has settled, instead of that later request being
        // silently dropped (see `expand()`'s own doc comment).
        const bool removed = beginChildReset(node);
        endChildReset(removed);
        node->childrenLoaded = false;
        node->hasError = false;
        node->errorText.clear();
        node->truncated = false;
        node->shownCount = -1;
        // Bump the generation so a reply for a fetch already queued/in flight
        // for this node -- belonging to the request being superseded -- is
        // recognised as stale and discarded rather than applied
        // (`m_activeGeneration`'s doc comment has the exact mechanism).
        // `queueFetch()` below both records this node's new generation and
        // (re)queues it; if a fetch for this node is already active, it stays
        // active (its own reply will be discarded when it lands) and this
        // queues the superseding one to run right after.
        ++m_generation;
    }

    switch (node->kind) {
    case NodeKind::Connection:
        ensureSessionOpen();
        queueFetch(node);
        break;
    case NodeKind::Schema:
        populateStaticGroups(node);
        break;
    case NodeKind::Group:
        ensureSessionOpen();
        queueFetch(node);
        break;
    case NodeKind::Object:
        if (node->isTableLike) {
            populateStaticColumnsNode(node);
        }
        break;
    case NodeKind::ColumnsNode:
        ensureSessionOpen();
        queueFetch(node);
        break;
    default:
        break;
    }
}

void ObjectBrowserModel::populateStaticGroups(Node *schemaNode)
{
    if (!schemaNode->children.empty()) {
        schemaNode->childrenLoaded = true;
        return;
    }
    const QModelIndex parentIdx = indexFor(schemaNode);
    beginInsertRows(parentIdx, 0, static_cast<int>(kGroups.size()) - 1);
    for (const GroupSpec &group : kGroups) {
        auto child = std::make_unique<Node>();
        child->kind = NodeKind::Group;
        child->parent = schemaNode;
        child->name = tr(group.label);
        child->objectKind = group.kind;
        child->schema = schemaNode->schema;
        schemaNode->children.push_back(std::move(child));
    }
    endInsertRows();
    schemaNode->childrenLoaded = true;
}

void ObjectBrowserModel::populateStaticColumnsNode(Node *objectNode)
{
    if (!objectNode->children.empty()) {
        objectNode->childrenLoaded = true;
        return;
    }
    const QModelIndex parentIdx = indexFor(objectNode);
    beginInsertRows(parentIdx, 0, 0);
    auto child = std::make_unique<Node>();
    child->kind = NodeKind::ColumnsNode;
    child->parent = objectNode;
    child->name = tr("Columns");
    child->schema = objectNode->schema;
    child->objectName = objectNode->objectName;
    objectNode->children.push_back(std::move(child));
    endInsertRows();
    objectNode->childrenLoaded = true;
}

void ObjectBrowserModel::setFilter(const QModelIndex &index, const QString &text)
{
    Node *node = nodeFor(index);
    if (node == nullptr) {
        return;
    }
    // `ColumnsOf` has no name filter, by design (metadata.rs:
    // `with_name_filter` panics on it) -- `Schema`/`Object`/`Column` are leaf
    // or synchronous nodes with nothing to filter either.
    if (node->kind != NodeKind::Connection && node->kind != NodeKind::Group) {
        return;
    }
    if (node->filterText == text) {
        return;
    }
    node->filterText = text;
    expand(index, /* force = */ true);
}

QString ObjectBrowserModel::filterFor(const QModelIndex &index) const
{
    Node *node = nodeFor(index);
    return node != nullptr ? node->filterText : QString();
}

void ObjectBrowserModel::queueFetch(Node *node)
{
    if (m_metadataSession == nullptr) {
        // `ensureSessionOpen()` (always called just before this) could not
        // even construct a session -- no bridge, or an invalid one. Fail
        // immediately rather than leaving the node showing "Loading..."
        // forever with nothing left to wake it.
        applyFetchError(node, RELDEX_ERROR_KIND_CONFIGURATION, 0, false, QString());
        return;
    }
    node->loading = true;
    node->hasError = false;
    node->errorText.clear();
    m_nodeGeneration[node] = m_generation;
    const QModelIndex idx = indexFor(node);
    if (idx.isValid()) {
        Q_EMIT dataChanged(idx, idx, { LoadingRole, HasErrorRole, StatusTextRole });
    }
    if (!m_pendingQueue.contains(node)) {
        m_pendingQueue.append(node);
    }
    startNextFetchIfIdle();
}

void ObjectBrowserModel::startNextFetchIfIdle()
{
    if (m_activeNode != nullptr || m_pendingQueue.isEmpty() || m_metadataSession == nullptr) {
        return;
    }

    if (m_sessionOpenFailed || m_metadataSession->state() == SessionController::Closed) {
        // The session itself never came up (or is gone): fail every queued
        // node with the session's own error rather than leaving them stuck
        // "loading" forever waiting for a retry that will not come on its
        // own. This is deliberately narrower than "the last statement
        // failed" -- see `m_sessionOpenFailed`'s own doc comment.
        const int kind = m_metadataSession->errorKind();
        const int nativeCode = m_metadataSession->errorNativeCode();
        const QString message = m_metadataSession->errorMessage();
        const QVector<Node *> queued = m_pendingQueue;
        m_pendingQueue.clear();
        for (Node *node : queued) {
            if (m_nodeGeneration.value(node, 0) == m_generation) {
                applyFetchError(node, kind, nativeCode, nativeCode != 0, message);
            }
        }
        return;
    }
    if (!m_sessionReady) {
        // Still connecting -- `onMetadataSessionOpened()` retries this once
        // the OPENED event actually lands. Deliberately NOT `isSessionOpen()`
        // (which only excludes Closed/Closing/open-failed): submitting
        // `execute()` this early races the connect and gets
        // `RELDEX_STATUS_INVALID_STATE` with no event ever following, per
        // `m_sessionReady`'s doc comment.
        return;
    }

    Node *node = m_pendingQueue.takeFirst();
    if (m_nodeGeneration.value(node, 0) != m_generation) {
        startNextFetchIfIdle(); // stale (superseded by a refresh/filter/close)
        return;
    }
    m_activeNode = node;
    // Captured once, here: the generation THIS fetch is submitted under.
    // `m_nodeGeneration[node]` may move on again while this fetch is still in
    // flight (a further refresh/filter change supersedes it); this frozen
    // copy is what the reply handlers compare against to tell "my own,
    // current reply" from "a reply for a request this node has moved past"
    // (`m_activeGeneration`'s own doc comment).
    m_activeGeneration = m_generation;

    ReldexMetadataRequest request = reldex::makeMetadataRequest();
    const QByteArray schemaUtf8 = node->schema.toUtf8();
    const QByteArray tableUtf8 = node->objectName.toUtf8();
    const QByteArray filterUtf8 = node->filterText.toUtf8();
    request.schema = toReldexStr(schemaUtf8);
    request.table = toReldexStr(tableUtf8);
    request.name_filter = toReldexStr(filterUtf8);
    request.has_name_filter = !node->filterText.isEmpty();
    request.limit = static_cast<std::uint32_t>(std::max(1, m_rowCap));

    switch (node->kind) {
    case NodeKind::Connection:
        request.kind = RELDEX_METADATA_REQUEST_KIND_SCHEMAS;
        break;
    case NodeKind::Group:
        request.kind = RELDEX_METADATA_REQUEST_KIND_OBJECTS_OF_KIND;
        request.object_kind = node->objectKind;
        break;
    case NodeKind::ColumnsNode:
        request.kind = RELDEX_METADATA_REQUEST_KIND_COLUMNS_OF;
        request.has_name_filter = false;
        break;
    default:
        Q_UNREACHABLE();
        break;
    }

    ReldexMetadataQuery *rawQuery = nullptr;
    const ReldexStatus prepared = reldex_metadata_prepare(&request, &rawQuery);
    if (prepared != RELDEX_STATUS_OK || rawQuery == nullptr) {
        // A prepare failure here is this request shape not being supported by
        // the composition root's driver choice -- a Reldex-internal fact, not
        // a database error (metadata.rs docs: only an unrecognized
        // MetadataObjectKind can cause this, and this class only ever builds
        // recognized kinds).
        m_activeNode = nullptr;
        applyFetchError(node, RELDEX_ERROR_KIND_DRIVER_INTERNAL, 0, false, QString());
        startNextFetchIfIdle();
        return;
    }
    m_activeQuery.reset(rawQuery);

    const QString sql = fromReldexStr(reldex_metadata_query_sql(rawQuery));
    ensureSessionOpen(); // no-op if already open; defensive against a stray call ordering
    const bool submitted = m_metadataSession->execute(sql);
    if (!submitted) {
        const int kind = m_metadataSession->errorKind();
        const int nativeCode = m_metadataSession->errorNativeCode();
        const QString message = m_metadataSession->errorMessage();
        m_activeQuery.reset();
        m_activeNode = nullptr;
        applyFetchError(node, kind, nativeCode, nativeCode != 0, message);
        startNextFetchIfIdle();
    }
    // Otherwise: wait for onMetadataResultComplete()/onMetadataSessionFailed().
}

void ObjectBrowserModel::onMetadataSessionFailed()
{
    if (m_activeNode == nullptr) {
        // Nothing was in flight, so this can only be the open/connect itself
        // failing (or a stray failure) -- never a per-statement failure,
        // which always has `m_activeNode` set. Latch it so the session is
        // abandoned rather than retried forever; a per-statement failure
        // below does NOT latch this, so the session stays usable for the
        // next, different fetch.
        m_sessionOpenFailed = true;
        Q_EMIT sessionOpenChanged();
        if (m_closePending) {
            performClose();
            return;
        }
        startNextFetchIfIdle();
        return;
    }

    Node *node = m_activeNode;
    // `m_activeGeneration`, not a fresh read of `m_nodeGeneration[node]`: the
    // latter is what the node's generation is NOW, which a refresh/filter
    // change may already have moved on from while this reply was in flight --
    // that is exactly the staleness this check exists to catch (see
    // `m_activeGeneration`'s own doc comment).
    const quint64 generation = m_activeGeneration;
    const int rawKind = m_metadataSession->errorKind();
    const int nativeCode = m_metadataSession->errorNativeCode();
    const bool hasNative = nativeCode != 0;
    const QString message = m_metadataSession->errorMessage();

    // Run the prepared query's own classifier (M2.8/M2.11): on Oracle this is
    // what turns ORA-00942/ORA-01039 -- indistinguishable from "really does
    // not exist" for ordinary SQL -- into `Permission` for *this* statement,
    // since it always names a dictionary object the driver itself chose,
    // which always exists (metadata.rs module docs, "Permission failures").
    int correctedKind = rawKind;
    if (m_activeQuery && rawKind != RELDEX_ERROR_KIND_UNKNOWN) {
        const QByteArray messageUtf8 = message.toUtf8();
        const reldex::ErrorHandle corrected(reldex_metadata_query_reclassify_error(
                m_activeQuery.get(), rawKind, nativeCode, hasNative, toReldexStr(messageUtf8)));
        if (corrected) {
            ReldexErrorView view = reldex::makeErrorView();
            if (reldex_error_view(corrected.get(), &view) == RELDEX_STATUS_OK) {
                correctedKind = view.kind;
            }
        }
    }

    m_activeQuery.reset();
    m_activeNode = nullptr;
    if (m_nodeGeneration.value(node, 0) == generation) {
        applyFetchError(node, correctedKind, nativeCode, hasNative, message);
    }
    // Otherwise: this reply belongs to a request `node` has moved past --
    // discarded silently, never shown. The fetch that superseded it is
    // already sitting in `m_pendingQueue` (`expand(force = true)` queued it
    // when it bumped the generation), so it is what the call below reaches.
    startNextFetchIfIdle();
}

void ObjectBrowserModel::onMetadataResultComplete()
{
    if (m_activeNode == nullptr) {
        return;
    }
    Node *node = m_activeNode;
    // See `onMetadataSessionFailed()`'s identical comment: `m_activeGeneration`
    // is the generation this specific reply belongs to, frozen at submission
    // time, not `node`'s current one.
    const quint64 generation = m_activeGeneration;

    ResultTableModel *result = m_metadataSession->model();
    const int rows = result != nullptr ? result->rowCount() : 0;
    const int cols = result != nullptr ? result->columnCount() : 0;

    // Metadata results are row-capped (`limit.get() + 1` at most, `metadata.rs`
    // module docs) -- at most a few hundred rows -- so pulling them out of the
    // formatted grid into plain strings here is the capped, small-result case
    // the architecture invariants allow, not the large-result path they ban.
    QVector<QStringList> values;
    values.reserve(rows);
    for (int row = 0; row < rows; ++row) {
        QStringList cellsInRow;
        cellsInRow.reserve(cols);
        for (int column = 0; column < cols; ++column) {
            cellsInRow.append(result->data(result->index(row, column), Qt::DisplayRole).toString());
        }
        values.append(cellsInRow);
    }

    m_activeQuery.reset();
    m_activeNode = nullptr;

    if (m_nodeGeneration.value(node, 0) == generation) {
        bool truncated = false;
        // `ColumnsOf` has no `limit` (a table's column count is already
        // server-bounded); truncation only ever applies to `Schemas`/
        // `ObjectsOfKind`.
        if ((node->kind == NodeKind::Connection || node->kind == NodeKind::Group)
            && values.size() > m_rowCap) {
            truncated = true;
            values.removeLast();
        }
        applyRows(node, values, truncated);
    }
    // Otherwise: stale (see `onMetadataSessionFailed()`) -- discarded, and the
    // superseding fetch already queued for this node is what runs next.
    startNextFetchIfIdle();
}

void ObjectBrowserModel::applyRows(Node *node, const QVector<QStringList> &rows, bool truncated)
{
    const QModelIndex ownIdx = indexFor(node);

    const bool removed = beginChildReset(node);
    endChildReset(removed);

    if (!rows.isEmpty()) {
        beginInsertRows(ownIdx, 0, static_cast<int>(rows.size()) - 1);
        for (const QStringList &values : rows) {
            auto child = std::make_unique<Node>();
            child->parent = node;
            switch (node->kind) {
            case NodeKind::Connection: // schemas_columns(): name, created
                child->kind = NodeKind::Schema;
                child->name = values.value(0);
                child->schema = values.value(0);
                break;
            case NodeKind::Group: { // objects_of_kind_columns(): name, status, created, last_modified
                child->kind = NodeKind::Object;
                child->name = values.value(0);
                child->schema = node->schema;
                child->objectName = values.value(0);
                child->objectKind = node->objectKind;
                child->isTableLike = node->objectKind == RELDEX_METADATA_OBJECT_KIND_TABLES
                        || node->objectKind == RELDEX_METADATA_OBJECT_KIND_VIEWS;
                const QString status = values.value(1);
                if (!status.isEmpty()
                    && status.compare(QStringLiteral("VALID"), Qt::CaseInsensitive) != 0) {
                    child->secondary = status;
                }
                break;
            }
            case NodeKind::ColumnsNode: // columns_of_columns(): position, name, type_name, nullable
                child->kind = NodeKind::Column;
                child->columnPosition = values.value(0);
                child->name = values.value(1);
                child->columnType = values.value(2);
                child->columnNullable = values.value(3);
                child->secondary = values.value(2);
                break;
            default:
                child->kind = NodeKind::Object;
                child->name = values.value(0);
                break;
            }
            node->children.push_back(std::move(child));
        }
        endInsertRows();
    }

    node->loading = false;
    node->childrenLoaded = true;
    node->hasError = false;
    node->errorText.clear();
    node->truncated = truncated;
    node->shownCount = static_cast<int>(rows.size());
    if (ownIdx.isValid()) {
        Q_EMIT dataChanged(ownIdx, ownIdx,
                          { LoadingRole, HasErrorRole, TruncatedRole, StatusTextRole });
    }

    if (node == m_columnsPaneNode) {
        updateColumnsPaneFrom(node);
    }
}

void ObjectBrowserModel::applyFetchError(Node *node, int errorKind, int nativeCode,
                                        bool hasNativeCode, const QString &message)
{
    Q_UNUSED(nativeCode);
    Q_UNUSED(hasNativeCode);
    Q_UNUSED(message); // never displayed: SPEC.md/AGENTS.md -- typed messages only, no raw driver text
    node->loading = false;
    node->hasError = true;
    node->errorText = describeError(errorKind);
    node->childrenLoaded = false; // a later expand()/refresh may retry
    const QModelIndex idx = indexFor(node);
    if (idx.isValid()) {
        Q_EMIT dataChanged(idx, idx, { LoadingRole, HasErrorRole, ErrorTextRole, StatusTextRole });
    }
    if (node == m_columnsPaneNode) {
        m_columnsPaneLoading = false;
        m_columnsPaneError = node->errorText;
        Q_EMIT columnsPaneChanged();
    }
}

QString ObjectBrowserModel::describeError(int errorKind)
{
    // A small, fixed table -- never the raw driver/native message
    // (`AGENTS.md`: keep driver-specific text out of the generic surface;
    // the task brief: "permission failures render as a permission message,
    // not a driver error; other errors as typed messages, never raw driver
    // text").
    switch (errorKind) {
    case RELDEX_ERROR_KIND_PERMISSION:
        return tr("You do not have permission to view this.");
    case RELDEX_ERROR_KIND_CONNECTION:
    case RELDEX_ERROR_KIND_NETWORK_LOST:
        return tr("The connection was lost.");
    case RELDEX_ERROR_KIND_TIMEOUT:
        return tr("The request timed out.");
    case RELDEX_ERROR_KIND_CANCELLED:
        return tr("The request was cancelled.");
    case RELDEX_ERROR_KIND_AUTHENTICATION:
        return tr("Authentication failed.");
    case RELDEX_ERROR_KIND_CONFIGURATION:
        return tr("The connection is not configured correctly.");
    case RELDEX_ERROR_KIND_UNSUPPORTED:
        return tr("This is not supported yet.");
    case RELDEX_ERROR_KIND_SYNTAX:
    case RELDEX_ERROR_KIND_CONSTRAINT:
    case RELDEX_ERROR_KIND_TRANSACTION:
    case RELDEX_ERROR_KIND_RESOURCE:
    case RELDEX_ERROR_KIND_DATA_CONVERSION:
    case RELDEX_ERROR_KIND_DRIVER_INTERNAL:
    case RELDEX_ERROR_KIND_OTHER:
    case RELDEX_ERROR_KIND_UNKNOWN:
    default:
        return tr("This could not be loaded.");
    }
}

void ObjectBrowserModel::activate(const QModelIndex &index)
{
    Node *node = nodeFor(index);
    if (node == nullptr) {
        return;
    }
    Node *columnsNode = nullptr;
    if (node->kind == NodeKind::ColumnsNode) {
        columnsNode = node;
    } else if (node->kind == NodeKind::Object && node->isTableLike) {
        if (!node->childrenLoaded) {
            populateStaticColumnsNode(node);
        }
        if (!node->children.empty()) {
            columnsNode = node->children.front().get();
        }
    }
    if (columnsNode == nullptr) {
        return;
    }

    m_columnsPaneNode = columnsNode;
    m_columnsPaneTitle = columnsNode->parent != nullptr
            ? QStringLiteral("%1.%2").arg(columnsNode->parent->schema, columnsNode->parent->objectName)
            : QString();

    if (columnsNode->childrenLoaded) {
        updateColumnsPaneFrom(columnsNode);
        return;
    }
    m_columnsPaneLoading = true;
    m_columnsPaneRows.clear();
    m_columnsPaneError.clear();
    Q_EMIT columnsPaneChanged();
    expand(indexFor(columnsNode));
}

void ObjectBrowserModel::updateColumnsPaneFrom(Node *columnsNode)
{
    m_columnsPaneRows.clear();
    for (const auto &child : columnsNode->children) {
        QVariantMap row;
        row[QStringLiteral("position")] = child->columnPosition;
        row[QStringLiteral("name")] = child->name;
        row[QStringLiteral("type")] = child->columnType;
        row[QStringLiteral("nullable")] = child->columnNullable;
        m_columnsPaneRows.append(row);
    }
    m_columnsPaneLoading = false;
    m_columnsPaneError = columnsNode->hasError ? columnsNode->errorText : QString();
    Q_EMIT columnsPaneChanged();
}
