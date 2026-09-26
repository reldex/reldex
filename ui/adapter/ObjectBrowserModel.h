#pragma once

// `ObjectBrowserModel` -- M6.1: the lazy tree over `SPEC.md` §16's object
// groups (connection -> schema -> the 9 group nodes -> objects -> a table or
// view's columns).
//
// Ownership and threading (ARCHITECTURE.md §7/§9, ADR-0002 "a worksheet owns
// a stable stateful session"): this model owns its **own** metadata session,
// opened lazily on first expand and closed on collapse/disconnect, entirely
// separate from any worksheet's `SessionController`. It never touches a
// worksheet's session and a worksheet never touches this one. Every fetch
// goes through the same async hub/event path every other session uses (no
// database or network I/O ever runs on the calling thread); the reply is
// drained by `Bridge::drain()` like any other session's, on the UI thread,
// which only ever *reads* an already-fetched batch's cells.
//
// # Known gap this class works around honestly (read before changing this file)
//
// As of M2.11 (ABI 3.1), the FFI cannot run a `PreparedMetadataQuery`
// end-to-end against a real database session:
//
//  * `reldex_session_execute()` takes only a bare SQL string -- "Binds are
//    not exported yet (M2.11)" (`crates/ffi/src/session.rs`, the doc comment
//    immediately above `reldex_session_execute`). Every metadata statement
//    has mandatory bind values (schema/table/name filter/limit,
//    `crates/db-driver-api/src/metadata.rs` module docs), so a prepared
//    metadata statement's binds can never reach the statement this class is
//    able to submit.
//  * Independently, `reldex_hub_open_session()` can only open the mock
//    driver's fixed `S14` scenario (`ReldexMockScenario` in `reldex.h` has no
//    other value, and `crates/ffi/src/mock.rs`'s `build_driver` -- the only
//    place a mock `Scenario` is built for an FFI-opened session -- never
//    calls `reldex_driver_mock::metadata::MetadataFixture`). There is no
//    build of this FFI, mock or otherwise, that can answer a metadata
//    statement today: `ReldexDriverKind` has no non-mock value at all yet
//    (`reldex.h`: "the only driver a build with the `mock-driver` feature can
//    open").
//
// So `prepare()`/`reclassify_error()` are real, wired to the real FFI, and
// exercised for real by this class and its tests. `execute()` on the
// metadata session is also real -- it is submitted through the same session
// hub as any other statement -- but until the two gaps above close, it can
// only ever come back with a typed, safe failure (never a crash, never wrong
// data): `ErrorKind::Other` from the mock ("no scripted response for
// statement ...") today, and `ErrorKind::Syntax`/a missing-bind error from a
// future real driver until bind support lands. This class treats that
// failure exactly like any other typed error it cannot special-case further:
// it surfaces the safe, generic message a `describeError()` produces, never
// the raw driver text. See `ui/README.md` "Object browser (M6.1)" for the
// full writeup and file/line references, and `docs/exec-plans/active/
// phase-1.md` row M6.1 for the hand-off.

#include "ReldexHandles.h"

#include <QAbstractItemModel>
#include <QHash>
#include <QPointer>
#include <QString>
#include <QVariantList>
#include <QVector>
#include <QtQml/qqmlregistration.h>

#include <memory>
#include <vector>

class Bridge;
class SessionController;

class ObjectBrowserModel : public QAbstractItemModel
{
    Q_OBJECT
    QML_ELEMENT

    /// The `Bridge` whose hub this model's own metadata session is opened
    /// against. Setting a new one closes any session already open.
    Q_PROPERTY(
            Bridge *bridge READ bridge WRITE setBridge NOTIFY bridgeChanged)
    /// The server-side row cap applied to every `Schemas`/`ObjectsOfKind`
    /// request (`SPEC.md` §16). Plain in-memory default, like
    /// `AppSettings::themeOverride` -- TODO(M2.9 wiring): once the settings
    /// model is reachable from the adapter, this should read/write through
    /// it instead of holding its own default.
    Q_PROPERTY(int rowCap READ rowCap WRITE setRowCap NOTIFY rowCapChanged)
    Q_PROPERTY(bool sessionOpen READ isSessionOpen NOTIFY sessionOpenChanged)
    Q_PROPERTY(quint64 sessionId READ sessionId NOTIFY sessionOpenChanged)
    /// True while this model's own metadata session is connecting. Exposed so
    /// the sidebar can show a spinner instead of an empty tree.
    Q_PROPERTY(bool sessionOpening READ isSessionOpening NOTIFY sessionOpenChanged)

    // --- the read-only columns pane (the last-activated table/view) --------
    Q_PROPERTY(QString columnsPaneTitle READ columnsPaneTitle NOTIFY columnsPaneChanged)
    Q_PROPERTY(QVariantList columnsPaneRows READ columnsPaneRows NOTIFY columnsPaneChanged)
    Q_PROPERTY(bool columnsPaneLoading READ columnsPaneLoading NOTIFY columnsPaneChanged)
    Q_PROPERTY(bool columnsPaneHasError READ columnsPaneHasError NOTIFY columnsPaneChanged)
    Q_PROPERTY(QString columnsPaneError READ columnsPaneError NOTIFY columnsPaneChanged)

public:
    enum Role {
        NameRole = Qt::UserRole + 1,
        SecondaryRole,
        KindRole,
        LoadingRole,
        HasErrorRole,
        ErrorTextRole,
        TruncatedRole,
        StatusTextRole,
    };
    Q_ENUM(Role)

    /// One tree level. `Root` is the model's own invisible root and never
    /// appears in a `QModelIndex` a view sees.
    enum class NodeKind {
        Root,
        Connection,
        Schema,
        Group,
        Object,
        ColumnsNode,
        Column,
    };
    Q_ENUM(NodeKind)

    explicit ObjectBrowserModel(QObject *parent = nullptr);
    ~ObjectBrowserModel() override;

    // --- QAbstractItemModel --------------------------------------------
    [[nodiscard]] QModelIndex index(int row, int column,
                                    const QModelIndex &parent = {}) const override;
    [[nodiscard]] QModelIndex parent(const QModelIndex &child) const override;
    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] bool hasChildren(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QHash<int, QByteArray> roleNames() const override;

    [[nodiscard]] Bridge *bridge() const noexcept { return m_bridge; }
    void setBridge(Bridge *bridge);
    [[nodiscard]] int rowCap() const noexcept { return m_rowCap; }
    void setRowCap(int cap);
    [[nodiscard]] bool isSessionOpen() const noexcept;
    [[nodiscard]] bool isSessionOpening() const noexcept;
    [[nodiscard]] quint64 sessionId() const noexcept;

    [[nodiscard]] QString columnsPaneTitle() const { return m_columnsPaneTitle; }
    [[nodiscard]] QVariantList columnsPaneRows() const { return m_columnsPaneRows; }
    [[nodiscard]] bool columnsPaneLoading() const noexcept { return m_columnsPaneLoading; }
    [[nodiscard]] bool columnsPaneHasError() const noexcept { return !m_columnsPaneError.isEmpty(); }
    [[nodiscard]] QString columnsPaneError() const { return m_columnsPaneError; }

    /// Fetches `index`'s children if they have never been loaded, or if
    /// `force` asks for a fresh fetch (a manual refresh, or `setFilter()`,
    /// which always forces). A no-op for a leaf or, when `!force`, for a node
    /// already loading or already loaded.
    ///
    /// `force` is safe to call on a node that is already loading -- that is
    /// the case it exists for. It **supersedes** whatever fetch is queued or
    /// already in flight for this node rather than being dropped by it: the
    /// node's generation is bumped (`m_generation`/`m_nodeGeneration`) and a
    /// fresh fetch is queued for the node's *current* filter text; a reply
    /// that lands for the superseded fetch is recognised as stale
    /// (`m_activeGeneration`) and discarded without being applied. The node
    /// keeps showing "Loading..." throughout -- there is no visible flicker
    /// back to whatever the superseded fetch would have shown. This is what
    /// makes "the newest request wins" true for a refresh or a filter change
    /// issued while a previous one for the same node has not settled yet; see
    /// `m_activeGeneration`'s own doc comment for the exact mechanism.
    ///
    /// Lazy loading (`SPEC.md` §16): nothing is fetched until this is called.
    Q_INVOKABLE void expand(const QModelIndex &index, bool force = false);
    /// Sets the server-side name filter for `index` (a `Connection` or
    /// `Group` node only) and re-fetches its children -- always as a `force`
    /// `expand()`, so a filter change while the previous one is still in
    /// flight supersedes it (see `expand()`'s doc comment) rather than being
    /// silently dropped: once the node settles, `filterFor(index)` always
    /// agrees with the filter that actually produced the displayed rows.
    /// Debouncing when the user is typing is the caller's job (presentation
    /// timing, not a business rule) -- see `ObjectBrowserPanel.qml`'s `Timer`.
    Q_INVOKABLE void setFilter(const QModelIndex &index, const QString &text);
    [[nodiscard]] Q_INVOKABLE QString filterFor(const QModelIndex &index) const;
    /// Activates `index` for the columns pane: if it is a table/view `Object`
    /// or its `ColumnsNode`, loads (or reuses) that table's columns.
    Q_INVOKABLE void activate(const QModelIndex &index);
    /// Closes this model's own metadata session, if one is open. Safe to call
    /// any number of times; called automatically from the destructor and
    /// whenever `bridge` changes.
    Q_INVOKABLE void closeConnection();

Q_SIGNALS:
    void bridgeChanged();
    void rowCapChanged();
    void sessionOpenChanged();
    void columnsPaneChanged();

private:
    // `tst_objectbrowsermodel.cpp` reaches `Node`/`populateStaticGroups()`/
    // `applyRows()`/`describeError()` directly for the tree-shape, row-cap
    // truncation and error-mapping behaviors that this class's own fetch
    // pipeline cannot currently reach end-to-end against any driver this
    // build can open (see this file's top-of-file doc comment on the FFI
    // gap): the test exercises the exact same private code the real pipeline
    // calls, rather than re-implementing it against a parallel fake.
    friend class tst_ObjectBrowserModel;

    struct Node
    {
        NodeKind kind = NodeKind::Root;
        Node *parent = nullptr;
        std::vector<std::unique_ptr<Node>> children;

        QString name;
        QString secondary;

        /// `Group`/`Object`: which of the 9 kinds. Unused otherwise.
        int objectKind = 0; // a ReldexMetadataObjectKind, 0 = n/a
        /// The schema this node's own objects (or this node itself) belong
        /// to: set on `Schema`, `Group`, `Object`, `ColumnsNode`.
        QString schema;
        /// `Object`/`ColumnsNode`: the table/view name columns belong to.
        QString objectName;
        /// `Object` only: whether this object kind has a `ColumnsNode` child
        /// (tables and views only, `SPEC.md` §16).
        bool isTableLike = false;
        /// `Column` only: `columns_of_columns()`'s `position`/`type_name`/
        /// `nullable` ("YES"/"NO"), kept apart from `name` so the columns
        /// pane can lay them out as separate fields.
        QString columnPosition;
        QString columnType;
        QString columnNullable;

        bool childrenLoaded = false;
        bool loading = false;
        bool hasError = false;
        QString errorText;
        bool truncated = false;
        int shownCount = -1;
        QString filterText;
    };

    [[nodiscard]] Node *nodeFor(const QModelIndex &index) const;
    [[nodiscard]] QModelIndex indexFor(Node *node, int column = 0) const;
    void ensureConnectionRoot();
    void ensureSessionOpen();
    /// Actually submits `closeSession()` on `m_metadataSession`. Only called
    /// once the session's fate is known (`m_sessionReady`, or it already
    /// failed to open) -- see `m_sessionReady`'s doc comment for why an
    /// earlier close would silently go nowhere.
    void performClose();
    void queueFetch(Node *node);
    void startNextFetchIfIdle();
    void populateStaticGroups(Node *schemaNode);
    void populateStaticColumnsNode(Node *objectNode);
    /// Removes and clears `node`'s current children, if any, correctly
    /// bracketed with `beginRemoveRows`/`endRemoveRows`. Returns whether a
    /// removal was actually begun (`endChildReset` must be told).
    [[nodiscard]] bool beginChildReset(Node *node);
    void endChildReset(bool removalStarted);
    void applyRows(Node *node, const QVector<QStringList> &rows, bool truncated);
    void applyFetchError(Node *node, int errorKind, int nativeCode, bool hasNativeCode,
                         const QString &message);
    [[nodiscard]] static QString describeError(int errorKind);
    void updateColumnsPaneFrom(Node *columnsNode);

    // --- metadata session lifecycle -----------------------------------
    void onMetadataSessionOpened();
    void onMetadataSessionFailed();
    void onMetadataResultComplete();
    void onMetadataSessionClosed();

    QPointer<Bridge> m_bridge;
    SessionController *m_metadataSession = nullptr;
    bool m_sessionClosing = false;
    /// Set only when the metadata session itself failed to come up (its
    /// `open()` call, or the async connect it started). Deliberately
    /// separate from `SessionController::state()`, which also becomes
    /// `Failed` after any single later statement fails -- a metadata session
    /// runs many independent statements over its life (one per expand), and
    /// one of them failing must not poison every fetch after it.
    bool m_sessionOpenFailed = false;
    /// True once the metadata session's `OPENED` event has actually been
    /// drained with no error -- i.e. the connect is *confirmed* done, not
    /// merely requested. Gates every `execute()`/`closeSession()` call this
    /// class makes: `reldex_session_execute`/`reldex_session_close` both
    /// answer `RELDEX_STATUS_INVALID_STATE` (submitting nothing, no event
    /// following) for a session still connecting (`reldex.h`, "Submitting
    /// before the session's OPENED event"), and `SessionController` does not
    /// retry or signal on that failure path -- calling either too early
    /// silently strands the caller with no reply ever coming. `sessionId()
    /// != 0` alone is NOT this signal: it is set synchronously by `open()`,
    /// before the connect itself is known to have finished.
    bool m_sessionReady = false;
    /// `closeConnection()` was asked for before `m_sessionReady` (or the open
    /// failing) was known; performed once one of those becomes true.
    bool m_closePending = false;

    std::unique_ptr<Node> m_root;

    /// Nodes waiting for their fetch to be submitted, FIFO. A node appears at
    /// most once (`queueFetch()` checks `contains()` first); `m_nodeGeneration`
    /// is what a queued node's own generation actually is, not its position
    /// here.
    QVector<Node *> m_pendingQueue;
    Node *m_activeNode = nullptr;
    reldex::MetadataQueryHandle m_activeQuery;
    /// What "current" means for a node's fetch. Bumped by `expand(force =
    /// true)` (a manual refresh, or `setFilter()`, which always forces) and by
    /// whatever invalidates every node at once (`setBridge()`, `closeConnection()`).
    /// `m_nodeGeneration[node]` is the generation *this node's most recently
    /// requested* fetch belongs to -- set every time `queueFetch(node)` runs,
    /// including when it supersedes a fetch already queued or already in
    /// flight for the same node (`expand(force = true)` no longer refuses to
    /// re-enter a loading node -- see its own doc comment).
    quint64 m_generation = 0;
    QHash<Node *, quint64> m_nodeGeneration;
    /// The generation `m_activeNode`'s *in-flight* fetch was actually
    /// submitted under -- captured once, in `startNextFetchIfIdle()`, at the
    /// moment `reldex_session_execute()` is called. This is deliberately a
    /// separate value from `m_nodeGeneration[m_activeNode]`, which can move on
    /// *while* that fetch is still outstanding (a refresh or a filter change
    /// arriving before its reply lands bumps `m_nodeGeneration[node]` again
    /// without touching this). `onMetadataResultComplete()`/
    /// `onMetadataSessionFailed()` compare the two: equal means the reply is
    /// for the node's current wish and is applied; unequal means the node
    /// moved on while this fetch was in flight, so the reply is discarded
    /// (never applied, never shown) and the fetch `queueFetch()` already
    /// queued for the newer generation is what actually runs next -- the
    /// "newest request wins" rule `setFilter()`'s and `expand(force = true)`'s
    /// own doc comments promise. Meaningless while `m_activeNode` is null.
    quint64 m_activeGeneration = 0;

    int m_rowCap = 500;

    Node *m_columnsPaneNode = nullptr;
    QString m_columnsPaneTitle;
    QVariantList m_columnsPaneRows;
    bool m_columnsPaneLoading = false;
    QString m_columnsPaneError;
};
