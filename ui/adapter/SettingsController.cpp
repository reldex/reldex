#include "SettingsController.h"

#include "ConnectionManager.h"

#include <QMetaObject>
#include <QProcessEnvironment>

#include <type_traits>
#include <utility>

/// The waker trampoline's body -- mirrors `reldexConnectionManagerWakeImpl`
/// (`ConnectionManager.cpp`) exactly; see that function's comment for the
/// rules both follow.
void reldexSettingsControllerWakeImpl(void *userData) noexcept
{
    auto *controller = static_cast<SettingsController *>(userData);
    if (controller == nullptr) {
        return;
    }
    try {
        if (controller->m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        QMetaObject::invokeMethod(controller, &SettingsController::drain, Qt::QueuedConnection);
    } catch (...) {
        controller->m_drainPosted.storeRelease(0);
    }
}

extern "C" {
static void reldexSettingsControllerWake(void *userData) noexcept
{
    reldexSettingsControllerWakeImpl(userData);
}
}

#ifdef RELDEX_HAVE_WORKSPACE_WAKE_FN_NOEXCEPT
static_assert(std::is_convertible_v<decltype(&reldexSettingsControllerWake),
                                     ReldexWorkspaceWakeFnNoexcept>,
              "the workspace waker trampoline must be noexcept (A18)");
#else
#error "reldex.h did not define RELDEX_HAVE_WORKSPACE_WAKE_FN_NOEXCEPT"
#endif

namespace {

/// 16 bytes, or a null pointer for an empty id -- what
/// `reldex_workspace_resolve_setting`'s `profile`/`worksheet` and
/// `reldex_workspace_set_setting`/`_clear_setting`'s `scope_id` all expect
/// for "no such layer"/"the application level".
class OptionalId
{
public:
    explicit OptionalId(const QByteArray &id) : m_bytes(id)
    {
        if (m_bytes.size() != 16) {
            m_bytes.clear();
        }
    }
    [[nodiscard]] const std::uint8_t *ptr() const noexcept
    {
        return m_bytes.isEmpty() ? nullptr
                                  : reinterpret_cast<const std::uint8_t *>(m_bytes.constData());
    }

private:
    QByteArray m_bytes;
};

} // namespace

SettingsController::SettingsController(QObject *parent) : QObject(parent) { }

SettingsController::~SettingsController()
{
    if (!m_workspace) {
        return;
    }
    // Mirrors ConnectionManager's D5-rule-2-style teardown: unregister the
    // waker first (blocks until an in-flight wake returns), then let the
    // handle's deleter close the workspace.
    reldex_workspace_set_waker(m_workspace.get(), nullptr, nullptr);
    m_workspace.reset();
}

bool SettingsController::open()
{
    if (m_workspace || m_openRequest != 0) {
        return true;
    }

    const QProcessEnvironment env = QProcessEnvironment::systemEnvironment();
    const bool forceInMemory = env.value(QStringLiteral("RELDEX_WORKSPACE_IN_MEMORY")) == "1";
    const bool forceMemoryCredentials =
            forceInMemory
            || env.value(QStringLiteral("RELDEX_WORKSPACE_MEMORY_CREDENTIAL_STORE")) == "1";
    QString path = env.value(QStringLiteral("RELDEX_WORKSPACE_PATH"));
    bool inMemory = forceInMemory;

    if (!inMemory && path.isEmpty()) {
        // Reuses `ConnectionManager`'s already-public, already-tested path
        // rule (ADR-0006 P5) rather than duplicating it -- this class does
        // not depend on a live `ConnectionManager` instance, just its two
        // pure static helpers.
        path = ConnectionManager::resolveDefaultStorePath(&ConnectionManager::defaultGetEnv);
        if (path.isEmpty()) {
            qWarning("SettingsController: no default workspace store path on this platform; "
                     "opening in-memory instead");
            inMemory = true;
        }
    }

    const QByteArray pathUtf8 = path.toUtf8();
    const ReldexStr pathStr = inMemory
            ? ReldexStr { nullptr, 0 }
            : ReldexStr { reinterpret_cast<const std::uint8_t *>(pathUtf8.constData()),
                          static_cast<std::size_t>(pathUtf8.size()) };
    const quint64 request = nextRequest();
    m_openRequest = request;

    ReldexWorkspace *workspace = nullptr;
    const ReldexStatus status = reldex_workspace_open(pathStr, inMemory, forceMemoryCredentials,
                                                       request, &workspace);
    if (status != RELDEX_STATUS_OK || workspace == nullptr) {
        qCritical("SettingsController: reldex_workspace_open() failed with status %d",
                  static_cast<int>(status));
        m_openRequest = 0;
        Q_EMIT openFinished(false);
        return false;
    }
    m_workspace.reset(workspace);

    const ReldexStatus wakerStatus =
            reldex_workspace_set_waker(m_workspace.get(), &reldexSettingsControllerWake, this);
    if (wakerStatus != RELDEX_STATUS_OK) {
        qCritical("SettingsController: reldex_workspace_set_waker() failed with status %d",
                  static_cast<int>(wakerStatus));
        m_workspace.reset();
        m_openRequest = 0;
        Q_EMIT openFinished(false);
        return false;
    }

    // A reply may already be queued by the time the waker is registered --
    // same reasoning as `ConnectionManager::open()`.
    drain();
    return true;
}

void SettingsController::postDrain()
{
    try {
        if (m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        QMetaObject::invokeMethod(this, &SettingsController::drain, Qt::QueuedConnection);
    } catch (...) {
        m_drainPosted.storeRelease(0);
        throw;
    }
}

void SettingsController::drain()
{
    m_drainPosted.storeRelease(0);
    if (!m_workspace) {
        return;
    }
    if (m_draining) {
        postDrain();
        return;
    }
    struct DrainGuard
    {
        bool &flag;
        explicit DrainGuard(bool &target) : flag(target) { flag = true; }
        ~DrainGuard() { flag = false; }
        DrainGuard(const DrainGuard &) = delete;
        DrainGuard &operator=(const DrainGuard &) = delete;
    } guard(m_draining);

    ReldexWorkspaceReply reply = reldex::makeWorkspaceReply();
    while (reldex_workspace_next_reply(m_workspace.get(), &reply)) {
        handleReply(reply);
        reldex::ErrorHandle owned(reply.error);
        reply = reldex::makeWorkspaceReply();
    }
}

quint64 SettingsController::nextRequest()
{
    return m_nextRequestId++;
}

void SettingsController::handleReply(const ReldexWorkspaceReply &reply)
{
    if (reply.kind == RELDEX_WORKSPACE_REPLY_KIND_OPENED) {
        if (reply.request != m_openRequest) {
            return;
        }
        m_openRequest = 0;
        const bool ok = reply.error == nullptr;
        if (ok) {
            m_ready = true;
            Q_EMIT readyChanged();
        }
        Q_EMIT openFinished(ok);
        return;
    }

    const auto pending = m_pending.find(reply.request);
    if (pending == m_pending.end()) {
        // Not one of ours (or already answered once -- the library promises
        // exactly one reply per request, same guarantee the hub makes).
        return;
    }
    const auto [kind, settingId] = pending.value();
    m_pending.erase(pending);

    QString message;
    if (reply.error != nullptr) {
        ReldexErrorView view = reldex::makeErrorView();
        if (reldex_error_view(reply.error, &view) == RELDEX_STATUS_OK) {
            message = QString::fromUtf8(reinterpret_cast<const char *>(view.message.ptr),
                                        static_cast<qsizetype>(view.message.len));
        }
    }

    switch (kind) {
    case PendingKind::SaveWorksheet:
        if (reply.error != nullptr) {
            Q_EMIT worksheetEnsureFailed(reply.request, message);
            return;
        }
        Q_EMIT worksheetEnsured(reply.request);
        return;
    case PendingKind::Resolve:
        if (reply.error != nullptr) {
            Q_EMIT resolveFailed(reply.request, settingId, message);
            return;
        }
        {
            Value value;
            value.kind = reply.setting_value.kind;
            value.boolValue = reply.setting_value.bool_value;
            value.countValue = reply.setting_value.count_value;
            value.noLimit = reply.setting_value.no_limit;
            value.numberValue = reply.setting_value.number_value;
            Q_EMIT resolved(reply.request, settingId, value, reply.setting_source);
        }
        return;
    case PendingKind::Set:
        if (reply.error != nullptr) {
            Q_EMIT setFailed(reply.request, settingId, message);
            return;
        }
        Q_EMIT valueSet(reply.request, settingId);
        return;
    case PendingKind::Clear:
        if (reply.error != nullptr) {
            Q_EMIT clearFailed(reply.request, settingId, message);
            return;
        }
        Q_EMIT valueCleared(reply.request, settingId);
        return;
    }
}

quint64 SettingsController::resolve(int settingId, const QByteArray &profileId,
                                    const QByteArray &worksheetId)
{
    if (!m_workspace) {
        Q_EMIT resolveFailed(0, settingId, QStringLiteral("the settings store is not open yet"));
        return 0;
    }
    const OptionalId profile(profileId);
    const OptionalId worksheet(worksheetId);
    const quint64 request = nextRequest();
    const ReldexStatus status = reldex_workspace_resolve_setting(
            m_workspace.get(), request, settingId, profile.ptr(), worksheet.ptr());
    if (status != RELDEX_STATUS_OK) {
        const reldex::ErrorHandle error(reldex_last_error_take());
        QString message;
        if (error) {
            ReldexErrorView view = reldex::makeErrorView();
            if (reldex_error_view(error.get(), &view) == RELDEX_STATUS_OK) {
                message = QString::fromUtf8(reinterpret_cast<const char *>(view.message.ptr),
                                            static_cast<qsizetype>(view.message.len));
            }
        }
        Q_EMIT resolveFailed(0, settingId, message);
        return 0;
    }
    m_pending.insert(request, qMakePair(PendingKind::Resolve, settingId));
    return request;
}

quint64 SettingsController::setValue(int settingId, int level, const QByteArray &scopeId,
                                     const Value &value)
{
    if (!m_workspace) {
        Q_EMIT setFailed(0, settingId, QStringLiteral("the settings store is not open yet"));
        return 0;
    }
    const OptionalId scope(scopeId);
    ReldexSettingValue wire = reldex::sized<ReldexSettingValue>();
    wire.kind = value.kind;
    wire.bool_value = value.boolValue;
    wire.count_value = value.countValue;
    wire.no_limit = value.noLimit;
    wire.number_value = value.numberValue;

    const quint64 request = nextRequest();
    const ReldexStatus status = reldex_workspace_set_setting(m_workspace.get(), request, settingId,
                                                              level, scope.ptr(), &wire);
    if (status != RELDEX_STATUS_OK) {
        Q_EMIT setFailed(0, settingId, QStringLiteral("the settings store refused the request"));
        return 0;
    }
    m_pending.insert(request, qMakePair(PendingKind::Set, settingId));
    return request;
}

quint64 SettingsController::clearValue(int settingId, int level, const QByteArray &scopeId)
{
    if (!m_workspace) {
        Q_EMIT clearFailed(0, settingId, QStringLiteral("the settings store is not open yet"));
        return 0;
    }
    const OptionalId scope(scopeId);
    const quint64 request = nextRequest();
    const ReldexStatus status =
            reldex_workspace_clear_setting(m_workspace.get(), request, settingId, level, scope.ptr());
    if (status != RELDEX_STATUS_OK) {
        Q_EMIT clearFailed(0, settingId, QStringLiteral("the settings store refused the request"));
        return 0;
    }
    m_pending.insert(request, qMakePair(PendingKind::Clear, settingId));
    return request;
}

quint64 SettingsController::ensureWorksheet(const QByteArray &worksheetId)
{
    if (!m_workspace) {
        Q_EMIT worksheetEnsureFailed(0, QStringLiteral("the settings store is not open yet"));
        return 0;
    }
    if (worksheetId.size() != 16) {
        Q_EMIT worksheetEnsureFailed(0, QStringLiteral("worksheet id must be 16 bytes"));
        return 0;
    }
    const quint64 request = nextRequest();
    const ReldexStr empty { nullptr, 0 };
    const ReldexStatus status = reldex_workspace_save_worksheet(
            m_workspace.get(), request,
            reinterpret_cast<const std::uint8_t *>(worksheetId.constData()), nullptr, empty, empty,
            0, 0, 0);
    if (status != RELDEX_STATUS_OK) {
        Q_EMIT worksheetEnsureFailed(0, QStringLiteral("the settings store refused the request"));
        return 0;
    }
    m_pending.insert(request, qMakePair(PendingKind::SaveWorksheet, RELDEX_SETTING_ID_UNKNOWN));
    return request;
}
