#include "ConnectionManager.h"

#include "Bridge.h"

#include <QByteArray>
#include <QDir>
#include <QMetaObject>
#include <QProcessEnvironment>
#include <QtGlobal>

#include <algorithm>
#include <type_traits>

namespace {

/// Builds a `ReldexStr` borrowing `text`'s bytes. `text` must outlive the
/// call the result is passed to -- the same "into Reldex" rule every other
/// `ReldexStr` in this adapter follows (`ReldexHandles.h`).
ReldexStr strOf(const QByteArray &text)
{
    return ReldexStr { reinterpret_cast<const std::uint8_t *>(text.constData()),
                       static_cast<std::size_t>(text.size()) };
}

QByteArray idToBytes16(const QByteArray &id)
{
    QByteArray bytes = id;
    bytes.resize(16); // pads with zeros if short; callers only ever pass 16
    return bytes;
}

} // namespace

/// The waker trampoline's body -- see `Bridge.cpp`'s `reldexBridgeWakeImpl`
/// for the rules this mirrors exactly (must not block, must not call any
/// `reldex_*` function, must not let a C++ exception escape).
void reldexConnectionManagerWakeImpl(void *userData) noexcept
{
    auto *manager = static_cast<ConnectionManager *>(userData);
    if (manager == nullptr) {
        return;
    }
    try {
        if (manager->m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        QMetaObject::invokeMethod(manager, &ConnectionManager::drain, Qt::QueuedConnection);
    } catch (...) {
        manager->m_drainPosted.storeRelease(0);
    }
}

extern "C" {
static void reldexConnectionManagerWake(void *userData) noexcept
{
    reldexConnectionManagerWakeImpl(userData);
}
}

#ifdef RELDEX_HAVE_WORKSPACE_WAKE_FN_NOEXCEPT
static_assert(std::is_convertible_v<decltype(&reldexConnectionManagerWake),
                                     ReldexWorkspaceWakeFnNoexcept>,
              "the workspace waker trampoline must be noexcept (A18)");
#else
#error "reldex.h did not define RELDEX_HAVE_WORKSPACE_WAKE_FN_NOEXCEPT"
#endif

// ============================================================================
// Construction / teardown
// ============================================================================

ConnectionManager::ConnectionManager(Bridge *bridge, QObject *parent)
    : QObject(parent), m_bridge(bridge)
{
    m_profiles = new ProfileModel(this);
    // Deliberately does not call open() here -- see the class documentation
    // and open()'s own doc comment for why opening is never automatic.
}

ConnectionManager::~ConnectionManager()
{
    if (!m_workspace) {
        return;
    }
    // Mirrors Bridge's D5-rule-2 ordering: unregister the waker first (it
    // does not return while a wake is in flight), so nothing can call back
    // into a half-destroyed object from here on. `reldex_workspace_close`
    // itself never blocks and must not be drained afterwards (reldex.h).
    reldex_workspace_set_waker(m_workspace.get(), nullptr, nullptr);
    // `m_bridge` (a QPointer, not a raw pointer -- see its declaration) is
    // reachable here with a Test Connect still in flight: nothing stops the
    // user closing the app while it is busy. `Bridge::~Bridge()` already
    // deletes this object explicitly, before its own `m_hubSinks` member is
    // torn down, but this check must hold even if that ever stops being
    // true -- a QPointer that has gone null is the one condition under which
    // `unregisterHubSink()` must not be called at all (round-2 fix,
    // 2026-09-26: a raw-pointer `m_bridge != nullptr` check here always
    // passed, even with `Bridge` mid-destruction, because a dangling raw
    // pointer is never null -- it crashed inside `Bridge::unregisterHubSink`,
    // reproduced 5/5 on MSVC).
    if (m_testConnectSessionId != 0 && m_bridge) {
        m_bridge->unregisterHubSink(m_testConnectSessionId);
    }
    m_workspace.reset();
}

// ============================================================================
// Opening the store. `resolveDefaultStorePath()` derives ADR-0006 P5's
// *path* only; the directory/file creation and Unix permissions themselves
// happen inside `reldex_workspace_open()`'s own service thread now
// (`Store::open_creating`, crates/ffi/src/workspace.rs) -- never here, on
// whichever thread calls `open()`.
// ============================================================================

QString ConnectionManager::defaultGetEnv(const char *name)
{
    return QProcessEnvironment::systemEnvironment().value(QString::fromLatin1(name));
}

QString ConnectionManager::resolveDefaultStorePath(const std::function<QString(const char *)> &getenv)
{
    auto absolute = [&](const char *name) -> QString {
        const QString value = getenv(name);
        if (value.isEmpty() || !QDir::isAbsolutePath(value)) {
            return {};
        }
        return value;
    };

#if defined(Q_OS_WIN)
    const QString base = absolute("LOCALAPPDATA");
    if (base.isEmpty()) {
        return {};
    }
#elif defined(Q_OS_MACOS)
    const QString home = absolute("HOME");
    if (home.isEmpty()) {
        return {};
    }
    const QString base = home + QStringLiteral("/Library/Application Support");
#elif defined(Q_OS_UNIX)
    QString base = absolute("XDG_DATA_HOME");
    if (base.isEmpty()) {
        const QString home = absolute("HOME");
        if (home.isEmpty()) {
            return {};
        }
        base = home + QStringLiteral("/.local/share");
    }
#else
    // No per-user data directory this platform layer can derive (Android,
    // iOS): the caller must pass an explicit path (ADR-0006 P5).
    return {};
#endif

    return base + QStringLiteral("/com.reldex.reldex/reldex.sqlite3");
}

bool ConnectionManager::open()
{
    if (m_workspace || m_openRequest != 0) {
        return true; // already open, or an open is already in flight
    }

    const QProcessEnvironment env = QProcessEnvironment::systemEnvironment();
    const bool forceInMemory = env.value(QStringLiteral("RELDEX_WORKSPACE_IN_MEMORY")) == "1";
    const bool forceMemoryCredentials =
            forceInMemory || env.value(QStringLiteral("RELDEX_WORKSPACE_MEMORY_CREDENTIAL_STORE")) == "1";
    QString path = env.value(QStringLiteral("RELDEX_WORKSPACE_PATH"));
    bool inMemory = forceInMemory;

    if (!inMemory && path.isEmpty()) {
        path = resolveDefaultStorePath();
        if (path.isEmpty()) {
            qWarning("ConnectionManager: no default workspace store path on this platform; "
                     "opening in-memory instead");
            inMemory = true;
        }
    }
    // No directory/file creation here any more: reldex_workspace_open()'s own
    // service thread does that now (Store::open_creating). Nothing in this
    // function does filesystem I/O, on this or any thread.

    const QByteArray pathUtf8 = path.toUtf8();
    const quint64 request = nextRequest();
    m_openRequest = request;

    ReldexWorkspace *workspace = nullptr;
    const ReldexStatus status = reldex_workspace_open(inMemory ? ReldexStr { nullptr, 0 }
                                                                : strOf(pathUtf8),
                                                       inMemory, forceMemoryCredentials, request,
                                                       &workspace);
    if (status != RELDEX_STATUS_OK || workspace == nullptr) {
        qCritical("reldex_workspace_open() failed with status %d; ConnectionManager reports "
                  "itself invalid",
                  static_cast<int>(status));
        m_openRequest = 0;
        Q_EMIT openFinished(false);
        return false;
    }
    m_workspace.reset(workspace);
    Q_EMIT validChanged();

    const ReldexStatus wakerStatus =
            reldex_workspace_set_waker(m_workspace.get(), &reldexConnectionManagerWake, this);
    if (wakerStatus != RELDEX_STATUS_OK) {
        qCritical("reldex_workspace_set_waker() failed with status %d; ConnectionManager reports "
                  "itself invalid rather than never delivering a reply",
                  static_cast<int>(wakerStatus));
        m_workspace.reset(); // no waker was registered
        Q_EMIT validChanged();
        m_openRequest = 0;
        Q_EMIT openFinished(false);
        return false;
    }

    // A reply (at least OPENED) may already be queued by the time the waker
    // is registered -- drain once now rather than waiting for a wake that may
    // never come if it arrived in the gap between open() and set_waker().
    drain();
    return true;
}

// ============================================================================
// Drain loop -- mirrors Bridge::drain(), scoped to one reply kind's worth of
// dispatch instead of a batch/error-owning `ReldexEvent`.
// ============================================================================

void ConnectionManager::postDrain()
{
    try {
        if (m_drainPosted.fetchAndStoreOrdered(1) != 0) {
            return;
        }
        QMetaObject::invokeMethod(this, &ConnectionManager::drain, Qt::QueuedConnection);
    } catch (...) {
        m_drainPosted.storeRelease(0);
        throw;
    }
}

void ConnectionManager::drain()
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

    // No event/time budget: unlike the hub, a workspace reply never owns a
    // large batch, and this queue's traffic is one dialog's worth of profile
    // edits, not a streaming result.
    ReldexWorkspaceReply reply = reldex::makeWorkspaceReply();
    while (reldex_workspace_next_reply(m_workspace.get(), &reply)) {
        handleReply(reply);
        reldex::ErrorHandle owned(reply.error);
        reldex::ProfileListHandle ownedList(reply.profile_list);
        reldex::ConnectSummaryHandle ownedSummary(reply.connect);
        reldex::SecretHandle ownedSecret(reply.secret);
        reply = reldex::makeWorkspaceReply();
    }
}

quint64 ConnectionManager::nextRequest()
{
    return m_nextRequestId++;
}

bool ConnectionManager::ensureReady()
{
    if (m_ready) {
        return true;
    }
    m_hasError = true;
    m_errorMessage = QStringLiteral("the workspace is not ready yet");
    m_errorMessageKey = QStringLiteral("error.workspace.notReady");
    m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;
    Q_EMIT errorChanged();
    return false;
}

void ConnectionManager::clearError()
{
    if (!m_hasError) {
        return;
    }
    m_hasError = false;
    m_errorMessage.clear();
    m_errorMessageKey.clear();
    m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;
    Q_EMIT errorChanged();
}

// ============================================================================
// Error mapping -- "each FFI error kind maps to a message key".
// ============================================================================

QString ConnectionManager::messageKeyForError(const ReldexErrorView &view, bool credentialContext)
{
    if (credentialContext && view.has_native) {
        switch (view.native_code) {
        case RELDEX_CREDENTIAL_ERROR_UNAVAILABLE:
            return QStringLiteral("error.credential.unavailable");
        case RELDEX_CREDENTIAL_ERROR_NOT_FOUND:
            return QStringLiteral("error.credential.notFound");
        case RELDEX_CREDENTIAL_ERROR_DENIED:
            return QStringLiteral("error.credential.denied");
        case RELDEX_CREDENTIAL_ERROR_TOO_LARGE:
            return QStringLiteral("error.credential.tooLarge");
        case RELDEX_CREDENTIAL_ERROR_INVALID_SECRET:
            return QStringLiteral("error.credential.invalidSecret");
        case RELDEX_CREDENTIAL_ERROR_MALFORMED:
            return QStringLiteral("error.credential.malformed");
        case RELDEX_CREDENTIAL_ERROR_LOCKED:
            return QStringLiteral("error.credential.locked");
        case RELDEX_CREDENTIAL_ERROR_BACKEND:
            return QStringLiteral("error.credential.backend");
        default:
            return QStringLiteral("error.credential.unknown");
        }
    }
    switch (view.kind) {
    case RELDEX_ERROR_KIND_CONFIGURATION:
        // Covers every `ProfileError` and `ConnectError` (including the
        // credential-pattern refusal) -- see the class documentation for why
        // this build cannot give a finer key than this one.
        return QStringLiteral("error.configuration");
    case RELDEX_ERROR_KIND_CONNECTION:
        return QStringLiteral("error.connection");
    case RELDEX_ERROR_KIND_AUTHENTICATION:
        return QStringLiteral("error.authentication");
    case RELDEX_ERROR_KIND_NETWORK_LOST:
        return QStringLiteral("error.networkLost");
    case RELDEX_ERROR_KIND_TIMEOUT:
        return QStringLiteral("error.timeout");
    case RELDEX_ERROR_KIND_CANCELLED:
        return QStringLiteral("error.cancelled");
    case RELDEX_ERROR_KIND_SYNTAX:
        return QStringLiteral("error.syntax");
    case RELDEX_ERROR_KIND_CONSTRAINT:
        return QStringLiteral("error.constraint");
    case RELDEX_ERROR_KIND_PERMISSION:
        return QStringLiteral("error.permission");
    case RELDEX_ERROR_KIND_TRANSACTION:
        return QStringLiteral("error.transaction");
    case RELDEX_ERROR_KIND_RESOURCE:
        return QStringLiteral("error.resource");
    case RELDEX_ERROR_KIND_DATA_CONVERSION:
        return QStringLiteral("error.dataConversion");
    case RELDEX_ERROR_KIND_UNSUPPORTED:
        return QStringLiteral("error.unsupported");
    case RELDEX_ERROR_KIND_DRIVER_INTERNAL:
        return QStringLiteral("error.driverInternal");
    case RELDEX_ERROR_KIND_OTHER:
        return QStringLiteral("error.other");
    default:
        return QStringLiteral("error.unknown");
    }
}

void ConnectionManager::adoptError(const ReldexError *error, bool credentialContext)
{
    ReldexErrorView view = reldex::makeErrorView();
    if (error == nullptr || !reldex_error_view(error, &view)) {
        m_hasError = true;
        m_errorMessage = QStringLiteral("an unknown error occurred");
        m_errorMessageKey = QStringLiteral("error.unknown");
        m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;
        Q_EMIT errorChanged();
        return;
    }
    m_hasError = true;
    m_errorMessage = QString::fromUtf8(reinterpret_cast<const char *>(view.message.ptr),
                                        static_cast<int>(view.message.len));
    m_errorMessageKey = messageKeyForError(view, credentialContext);
    m_errorKind = view.kind;
    Q_EMIT errorChanged();
}

// ============================================================================
// Field mapping: QVariantMap (QML) <-> ReldexProfileDetails (FFI) <->
// ProfileModel::Row (list display). All in this class, per AGENTS.md ("no
// business rules in QML").
// ============================================================================

QByteArray ConnectionManager::idBytes(const QString &idHex)
{
    return idToBytes16(QByteArray::fromHex(idHex.toLatin1()));
}

QString ConnectionManager::idHexOf(const QByteArray &id16)
{
    return QString::fromLatin1(id16.toHex());
}

ConnectionManager::ProfileFields ConnectionManager::fieldsFromMap(const QVariantMap &map)
{
    ProfileFields fields;
    fields.name = map.value(QStringLiteral("name")).toString();
    fields.environment =
            map.value(QStringLiteral("environment"), RELDEX_ENVIRONMENT_KIND_DEVELOPMENT).toInt();
    fields.environmentLabel = map.value(QStringLiteral("environmentLabel")).toString();
    fields.treatAsProduction = map.value(QStringLiteral("treatAsProduction")).toBool();
    fields.endpointKind =
            map.value(QStringLiteral("endpointKind"), RELDEX_ENDPOINT_KIND_HOST_PORT).toInt();
    fields.host = map.value(QStringLiteral("host")).toString();
    fields.port = map.value(QStringLiteral("port")).toInt();
    fields.serviceTargetKind = map.value(QStringLiteral("serviceTargetKind"),
                                          RELDEX_SERVICE_TARGET_KIND_SERVICE_NAME)
                                        .toInt();
    fields.serviceNameOrSid = map.value(QStringLiteral("serviceNameOrSid")).toString();
    fields.connectString = map.value(QStringLiteral("connectString")).toString();
    fields.authKind = map.value(QStringLiteral("authKind"), RELDEX_AUTH_KIND_PASSWORD).toInt();
    fields.username = map.value(QStringLiteral("username")).toString();
    fields.passwordStorage = map.value(QStringLiteral("passwordStorage"),
                                        RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME)
                                      .toInt();
    fields.role = map.value(QStringLiteral("sessionRole"), RELDEX_SESSION_ROLE_KIND_NORMAL).toInt();
    fields.transport = map.value(QStringLiteral("transport"), RELDEX_TRANSPORT_KIND_PLAIN).toInt();
    fields.caDirectory = map.value(QStringLiteral("caDirectory")).toString();
    fields.allowUnenforcedCertificatePin =
            map.value(QStringLiteral("allowUnenforcedCertificatePin")).toBool();
    return fields;
}

ConnectionManager::OwnedProfileDetails ConnectionManager::buildDetails(const ProfileFields &fields)
{
    OwnedProfileDetails owned;
    owned.name = fields.name.toUtf8();
    owned.environmentLabel = fields.environmentLabel.toUtf8();
    owned.host = fields.host.toUtf8();
    owned.serviceNameOrSid = fields.serviceNameOrSid.toUtf8();
    owned.connectString = fields.connectString.toUtf8();
    owned.username = fields.username.toUtf8();
    owned.caDirectory = fields.caDirectory.toUtf8();

    owned.details = reldex::makeProfileDetails();
    owned.details.name = strOf(owned.name);
    owned.details.database_type = RELDEX_DATABASE_TYPE_ORACLE;
    owned.details.environment = fields.environment;
    owned.details.environment_label = strOf(owned.environmentLabel);
    owned.details.treat_as_production = fields.treatAsProduction;
    owned.details.endpoint_kind = fields.endpointKind;
    owned.details.host = strOf(owned.host);
    owned.details.port = static_cast<std::uint16_t>(std::clamp(fields.port, 0, 65535));
    owned.details.service_target_kind = fields.serviceTargetKind;
    owned.details.service_name_or_sid = strOf(owned.serviceNameOrSid);
    owned.details.connect_string = strOf(owned.connectString);
    owned.details.auth_kind = fields.authKind;
    owned.details.username = strOf(owned.username);
    owned.details.password_storage = fields.passwordStorage;
    owned.details.role = fields.role;
    owned.details.transport = fields.transport;
    owned.details.ca_directory = strOf(owned.caDirectory);
    owned.details.allow_unenforced_certificate_pin = fields.allowUnenforcedCertificatePin;
    return owned;
}

QString ConnectionManager::endpointSummaryFor(const ReldexProfileView &view)
{
    auto asString = [](const ReldexStr &text) {
        return QString::fromUtf8(reinterpret_cast<const char *>(text.ptr),
                                  static_cast<int>(text.len));
    };
    if (view.endpoint_kind == RELDEX_ENDPOINT_KIND_HOST_PORT) {
        const QString target = view.service_target_kind == RELDEX_SERVICE_TARGET_KIND_SID
                ? QStringLiteral("SID %1").arg(asString(view.service_name_or_sid))
                : asString(view.service_name_or_sid);
        return QStringLiteral("%1:%2/%3").arg(asString(view.host)).arg(view.port).arg(target);
    }
    return asString(view.connect_string);
}

ProfileModel::Row ConnectionManager::rowFromView(const ReldexProfileView &view)
{
    auto asString = [](const ReldexStr &text) {
        return QString::fromUtf8(reinterpret_cast<const char *>(text.ptr),
                                  static_cast<int>(text.len));
    };
    ProfileModel::Row row;
    row.id = QByteArray(reinterpret_cast<const char *>(view.id), 16);
    row.name = asString(view.name);
    row.environment = view.environment;
    row.environmentLabel = asString(view.environment_label);
    row.treatAsProduction = view.treat_as_production;
    row.endpointSummary = endpointSummaryFor(view);
    row.endpointKind = view.endpoint_kind;
    row.host = asString(view.host);
    row.port = view.port;
    row.serviceTargetKind = view.service_target_kind;
    row.serviceNameOrSid = asString(view.service_name_or_sid);
    row.connectString = asString(view.connect_string);
    row.username = asString(view.username);
    row.authKind = view.auth_kind;
    row.passwordStorage = view.password_storage;
    row.databaseType = view.database_type;
    row.sessionRole = view.role;
    row.transport = view.transport;
    row.caDirectory = asString(view.ca_directory);
    row.allowUnenforcedCertificatePin = view.allow_unenforced_certificate_pin;
    return row;
}

// ============================================================================
// Commands
// ============================================================================

bool ConnectionManager::createProfile(const QVariantMap &fieldsMap)
{
    clearError();
    if (!ensureReady()) {
        return false;
    }
    ProfileFields fields = fieldsFromMap(fieldsMap);
    const QString password = fieldsMap.value(QStringLiteral("password")).toString();

    PendingOp op;
    op.kind = OpKind::CreateProfile;
    op.fields = fields;
    op.passwordToPut = password;
    op.finalPasswordStorage = fields.passwordStorage;
    // A profile never claims a password the store does not hold yet (ADR-0007
    // write order): create it as prompt-each-time first regardless of what
    // was requested, then flip the flag only once `put` has actually
    // succeeded.
    op.fields.passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
    op.step = Step::CreateAwaitingSave;

    const OwnedProfileDetails details = buildDetails(op.fields);
    const quint64 request = nextRequest();
    const ReldexStatus status =
            reldex_workspace_create_profile(m_workspace.get(), request, &details.details);
    if (status != RELDEX_STATUS_OK) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
        return false;
    }
    m_pending.insert(request, op);
    return true;
}

void ConnectionManager::continueCreateProfile(quint64 request, PendingOp &op,
                                               const ReldexWorkspaceReply &reply)
{
    switch (op.step) {
    case Step::CreateAwaitingSave: {
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        op.profileId = QByteArray(reinterpret_cast<const char *>(reply.id), 16);
        const bool wantsSave =
                op.finalPasswordStorage == RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE
                && !op.passwordToPut.isEmpty();
        if (!wantsSave) {
            finishSave(op, op.profileId, true);
            m_pending.remove(request);
            return;
        }
        const QByteArray passwordUtf8 = op.passwordToPut.toUtf8();
        const quint64 putRequest = nextRequest();
        const QByteArray id16 = idToBytes16(op.profileId);
        const ReldexStatus status = reldex_workspace_credential_put(
                m_workspace.get(), putRequest,
                reinterpret_cast<const std::uint8_t *>(id16.constData()), strOf(passwordUtf8));
        if (status != RELDEX_STATUS_OK) {
            // The profile is already saved as prompt-each-time; report the
            // password-save failure as a warning, not a save failure.
            adoptError(reldex_last_error_take(), false);
            Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            finishSave(op, op.profileId, true);
            m_pending.remove(request);
            return;
        }
        op.step = Step::CreateAwaitingCredentialPut;
        m_pending.remove(request);
        m_pending.insert(putRequest, op);
        return;
    }
    case Step::CreateAwaitingCredentialPut: {
        const bool putFailed = reply.error != nullptr;
        if (putFailed) {
            adoptError(reply.error, true);
            Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            // ADR-0007 write order: `put` failed, so the flag stays
            // prompt-each-time -- which is exactly what the profile already
            // carries from step one. Nothing more to save.
            finishSave(op, op.profileId, true);
            m_pending.remove(request);
            return;
        }
        op.fields.passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
        const OwnedProfileDetails details = buildDetails(op.fields);
        const QByteArray id16 = idToBytes16(op.profileId);
        const quint64 updateRequest = nextRequest();
        const ReldexStatus status = reldex_workspace_update_profile(
                m_workspace.get(), updateRequest,
                reinterpret_cast<const std::uint8_t *>(id16.constData()), &details.details);
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            finishSave(op, op.profileId, true);
            m_pending.remove(request);
            return;
        }
        op.step = Step::CreateAwaitingFlagUpdate;
        m_pending.remove(request);
        m_pending.insert(updateRequest, op);
        return;
    }
    case Step::CreateAwaitingFlagUpdate: {
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
        }
        finishSave(op, op.profileId, true);
        m_pending.remove(request);
        return;
    }
    default:
        m_pending.remove(request);
        return;
    }
}

bool ConnectionManager::updateProfile(const QString &idHex, const QVariantMap &fieldsMap)
{
    clearError();
    if (!ensureReady()) {
        return false;
    }
    const QByteArray id16 = idBytes(idHex);
    const ProfileModel::Row *existing = m_profiles->findRow(id16);
    const int previousPasswordStorage =
            existing != nullptr ? existing->passwordStorage : RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
    const int previousAuthKind =
            existing != nullptr ? existing->authKind : RELDEX_AUTH_KIND_PASSWORD;

    ProfileFields fields = fieldsFromMap(fieldsMap);
    const QString password = fieldsMap.value(QStringLiteral("password")).toString();
    const bool wantsStored = fields.passwordStorage == RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE
            && fields.authKind == RELDEX_AUTH_KIND_PASSWORD;
    const bool hadStored = previousPasswordStorage == RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE
            && previousAuthKind == RELDEX_AUTH_KIND_PASSWORD;

    PendingOp op;
    op.kind = OpKind::UpdateProfile;
    op.profileId = id16;
    op.fields = fields;
    op.finalPasswordStorage = fields.passwordStorage;

    if (wantsStored && !password.isEmpty()) {
        // Replacing the saved password (or saving one for the first time):
        // put first, per the ADR-0007 write order.
        op.passwordToPut = password;
        op.fields.passwordStorage = previousPasswordStorage; // unchanged until `put` resolves
        op.step = Step::UpdateAwaitingCredentialPut;
        const QByteArray passwordUtf8 = password.toUtf8();
        const quint64 request = nextRequest();
        const ReldexStatus status = reldex_workspace_credential_put(
                m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()),
                strOf(passwordUtf8));
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            return false;
        }
        m_pending.insert(request, op);
        return true;
    }

    if (!wantsStored && hadStored) {
        // Switching away from a saved password (to prompt-each-time, or to
        // external authentication): clear the stored entry first (ADR-0007
        // "Clearing"), then flip the flag regardless of that outcome.
        op.step = Step::UpdateAwaitingCredentialDelete;
        const quint64 request = nextRequest();
        const ReldexStatus status = reldex_workspace_credential_delete(
                m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()));
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            return false;
        }
        m_pending.insert(request, op);
        return true;
    }

    // Nothing credential-shaped changed (kept prompt-each-time, or kept the
    // existing stored password with no new one typed): a plain save.
    op.step = Step::UpdateAwaitingSave;
    const OwnedProfileDetails details = buildDetails(op.fields);
    const quint64 request = nextRequest();
    const ReldexStatus status = reldex_workspace_update_profile(
            m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()),
            &details.details);
    if (status != RELDEX_STATUS_OK) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
        return false;
    }
    m_pending.insert(request, op);
    return true;
}

void ConnectionManager::continueUpdateProfile(quint64 request, PendingOp &op,
                                               const ReldexWorkspaceReply &reply)
{
    switch (op.step) {
    case Step::UpdateAwaitingCredentialPut: {
        if (reply.error != nullptr) {
            adoptError(reply.error, true);
            Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            // Write order: `put` failed, so the flag must not move to
            // CredentialStore -- fall back to prompt-each-time explicitly
            // rather than leaving whatever the profile had before, so the UI
            // never implies a password is saved when it is not.
            op.fields.passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
        } else {
            op.fields.passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
        }
        const OwnedProfileDetails details = buildDetails(op.fields);
        const QByteArray id16 = idToBytes16(op.profileId);
        const quint64 saveRequest = nextRequest();
        const ReldexStatus status = reldex_workspace_update_profile(
                m_workspace.get(), saveRequest,
                reinterpret_cast<const std::uint8_t *>(id16.constData()), &details.details);
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        op.step = Step::UpdateAwaitingSave;
        m_pending.remove(request);
        m_pending.insert(saveRequest, op);
        return;
    }
    case Step::UpdateAwaitingCredentialDelete: {
        if (reply.error != nullptr) {
            ReldexErrorView view = reldex::makeErrorView();
            const bool decoded = reldex_error_view(reply.error, &view);
            const bool countsAsDone =
                    decoded && view.has_native && view.native_code == RELDEX_CREDENTIAL_ERROR_UNAVAILABLE;
            if (!countsAsDone) {
                adoptError(reply.error, true);
                Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            }
        }
        op.fields.passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
        const OwnedProfileDetails details = buildDetails(op.fields);
        const QByteArray id16 = idToBytes16(op.profileId);
        const quint64 saveRequest = nextRequest();
        const ReldexStatus status = reldex_workspace_update_profile(
                m_workspace.get(), saveRequest,
                reinterpret_cast<const std::uint8_t *>(id16.constData()), &details.details);
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        op.step = Step::UpdateAwaitingSave;
        m_pending.remove(request);
        m_pending.insert(saveRequest, op);
        return;
    }
    case Step::UpdateAwaitingSave: {
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            Q_EMIT profileSaveFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        finishSave(op, op.profileId, false);
        m_pending.remove(request);
        return;
    }
    default:
        m_pending.remove(request);
        return;
    }
}

void ConnectionManager::finishSave(const PendingOp &op, const QByteArray &id, bool created)
{
    const QByteArray id16 = idToBytes16(id);
    // A save reply does not carry a full profile view back, so the row is
    // built straight from what was just written -- which is exactly what the
    // store now holds, because every step above wrote it, in order, on this
    // one service thread.
    ProfileModel::Row row;
    row.id = id16;
    row.name = op.fields.name;
    row.environment = op.fields.environment;
    row.environmentLabel = op.fields.environmentLabel;
    row.treatAsProduction = op.fields.treatAsProduction;
    row.endpointKind = op.fields.endpointKind;
    row.host = op.fields.host;
    row.port = op.fields.port;
    row.serviceTargetKind = op.fields.serviceTargetKind;
    row.serviceNameOrSid = op.fields.serviceNameOrSid;
    row.connectString = op.fields.connectString;
    row.username = op.fields.username;
    row.authKind = op.fields.authKind;
    row.passwordStorage = op.fields.passwordStorage;
    row.databaseType = RELDEX_DATABASE_TYPE_ORACLE;
    row.sessionRole = op.fields.role;
    row.transport = op.fields.transport;
    row.caDirectory = op.fields.caDirectory;
    row.allowUnenforcedCertificatePin = op.fields.allowUnenforcedCertificatePin;
    row.endpointSummary = row.endpointKind == RELDEX_ENDPOINT_KIND_HOST_PORT
            ? QStringLiteral("%1:%2/%3")
                      .arg(row.host)
                      .arg(row.port)
                      .arg(row.serviceTargetKind == RELDEX_SERVICE_TARGET_KIND_SID
                                   ? QStringLiteral("SID %1").arg(row.serviceNameOrSid)
                                   : row.serviceNameOrSid)
            : row.connectString;
    m_profiles->upsertRow(row);
    Q_EMIT profileSaved(idHexOf(id16), created);
}

bool ConnectionManager::deleteProfile(const QString &idHex)
{
    clearError();
    if (!ensureReady()) {
        return false;
    }
    const QByteArray id16 = idBytes(idHex);
    PendingOp op;
    op.kind = OpKind::DeleteProfile;
    op.profileId = id16;
    op.step = Step::DeleteAwaitingCredentialDelete;

    const quint64 request = nextRequest();
    const ReldexStatus status = reldex_workspace_credential_delete(
            m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()));
    if (status != RELDEX_STATUS_OK) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT profileDeleteFailed(m_errorMessageKey, m_errorMessage);
        return false;
    }
    m_pending.insert(request, op);
    return true;
}

void ConnectionManager::continueDeleteProfile(quint64 request, PendingOp &op,
                                               const ReldexWorkspaceReply &reply)
{
    switch (op.step) {
    case Step::DeleteAwaitingCredentialDelete: {
        if (reply.error != nullptr) {
            ReldexErrorView view = reldex::makeErrorView();
            const bool decoded = reldex_error_view(reply.error, &view);
            const bool countsAsDone =
                    decoded && view.has_native && view.native_code == RELDEX_CREDENTIAL_ERROR_UNAVAILABLE;
            if (!countsAsDone) {
                adoptError(reply.error, true);
                Q_EMIT credentialWarning(m_errorMessageKey, m_errorMessage);
            }
        }
        const QByteArray id16 = idToBytes16(op.profileId);
        const quint64 deleteRequest = nextRequest();
        const ReldexStatus status = reldex_workspace_delete_profile(
                m_workspace.get(), deleteRequest, reinterpret_cast<const std::uint8_t *>(id16.constData()));
        if (status != RELDEX_STATUS_OK) {
            adoptError(reldex_last_error_take(), false);
            Q_EMIT profileDeleteFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        op.step = Step::DeleteAwaitingDelete;
        m_pending.remove(request);
        m_pending.insert(deleteRequest, op);
        return;
    }
    case Step::DeleteAwaitingDelete: {
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            Q_EMIT profileDeleteFailed(m_errorMessageKey, m_errorMessage);
            m_pending.remove(request);
            return;
        }
        const QByteArray id16 = idToBytes16(op.profileId);
        m_profiles->removeRow(id16);
        Q_EMIT profileDeleted(idHexOf(id16));
        m_pending.remove(request);
        return;
    }
    default:
        m_pending.remove(request);
        return;
    }
}

// ============================================================================
// Reply dispatch
// ============================================================================

void ConnectionManager::handleReply(const ReldexWorkspaceReply &reply)
{
    if (reply.request == m_openRequest && m_openRequest != 0 && !m_ready) {
        m_openRequest = 0;
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            Q_EMIT openFinished(false);
            return;
        }
        m_ready = true;
        m_credentialStoreKind = reldex_workspace_credential_store_kind(m_workspace.get());
        Q_EMIT readyChanged();
        Q_EMIT openFinished(true);
        // Populate the model immediately so a freshly opened dialog is never
        // empty for no reason the user can see.
        reldex_workspace_list_profiles(m_workspace.get(), nextRequest());
        return;
    }

    if (reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PROFILES_LISTED) {
        if (reply.error != nullptr) {
            adoptError(reply.error, false);
            return;
        }
        QVector<ProfileModel::Row> rows;
        const std::size_t count = reldex_profile_list_count(reply.profile_list);
        rows.reserve(static_cast<int>(count));
        for (std::size_t i = 0; i < count; ++i) {
            ReldexProfileView view = reldex::makeProfileView();
            if (reldex_profile_list_get(reply.profile_list, i, &view)) {
                rows.append(rowFromView(view));
            }
        }
        m_profiles->resetRows(std::move(rows));
        return;
    }

    if (reply.kind == RELDEX_WORKSPACE_REPLY_KIND_CONNECT_PARAMS_BUILT) {
        handleConnectParamsBuilt(reply);
        return;
    }

    if (reply.kind == RELDEX_WORKSPACE_REPLY_KIND_PASSWORD_RESOLVED) {
        handlePasswordResolved(reply);
        return;
    }

    const auto pendingIt = m_pending.find(reply.request);
    if (pendingIt == m_pending.end()) {
        return; // e.g. the initial list_profiles request, already handled above
    }
    PendingOp op = pendingIt.value();
    switch (op.kind) {
    case OpKind::CreateProfile:
        continueCreateProfile(reply.request, op, reply);
        break;
    case OpKind::UpdateProfile:
        continueUpdateProfile(reply.request, op, reply);
        break;
    case OpKind::DeleteProfile:
        continueDeleteProfile(reply.request, op, reply);
        break;
    }
}

// ============================================================================
// Test-connect
// ============================================================================

bool ConnectionManager::testConnect(const QString &idHex, const QString &password)
{
    clearError();
    if (!ensureReady()) {
        return false;
    }
    if (m_testConnectBusy) {
        return false;
    }
    m_testConnectBusy = true;
    m_testConnectProfileId = idBytes(idHex);
    m_testConnectSessionId = 0;
    m_testConnectSummary.clear();
    m_testConnectUsedStoredPassword = false;
    Q_EMIT testConnectStateChanged();

    if (!password.isEmpty()) {
        // An explicit, just-typed password always wins and always bypasses
        // the credential store -- this is a *different* attempt from
        // whatever the store might hold, never "the stored password", so it
        // can never be the thing `handleHubEvent()` offers to update.
        const QByteArray passwordUtf8 = password.toUtf8();
        reldex::SecretHandle secret(reldex_secret_from_utf8(strOf(passwordUtf8)));
        const bool issued = issueBuildConnectParams(idHex, secret.get());
        // Not consumed by the call above (reldex.h): released here,
        // immediately, rather than kept until the async reply.
        secret.reset();
        return issued;
    }

    // No password typed: ask `resolve_password` where one would come from
    // rather than trusting the profile's own `passwordStorage` flag locally
    // (that flag can be stale -- e.g. a `credential_put` that failed after
    // create leaves it at prompt-each-time even though a stale entry might
    // still exist). `resolve_password` is ADR-0007 S3's single source of
    // truth; only when it resolves `FromStore` does a later authentication
    // failure get to offer "update the stored password?" in
    // `handleHubEvent()`.
    const quint64 request = nextRequest();
    const QByteArray id16 = idToBytes16(m_testConnectProfileId);
    const ReldexStatus status = reldex_workspace_resolve_password(
            m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()));
    if (status != RELDEX_STATUS_OK) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return false;
    }
    m_testConnectResolveRequest = request;
    return true;
}

void ConnectionManager::handlePasswordResolved(const ReldexWorkspaceReply &reply)
{
    if (reply.request != m_testConnectResolveRequest || !m_testConnectBusy) {
        return;
    }
    m_testConnectResolveRequest = 0;
    const QString idHex = idHexOf(idToBytes16(m_testConnectProfileId));
    if (reply.error != nullptr) {
        adoptError(reply.error, false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return;
    }
    m_testConnectUsedStoredPassword =
            reply.password_source_kind == RELDEX_PASSWORD_SOURCE_KIND_FROM_STORE;
    // `reply.secret` (null unless FromStore) is passed on synchronously, in
    // this same drain iteration; `drain()`'s own SecretHandle releases it
    // right after handleReply() returns -- never adopted or retained here,
    // the same rule every other reply-owned object in this class follows.
    issueBuildConnectParams(idHex, reply.secret);
}

bool ConnectionManager::issueBuildConnectParams(const QString &idHex, const ReldexSecret *password)
{
    const quint64 request = nextRequest();
    const QByteArray id16 = idToBytes16(m_testConnectProfileId);
    const ReldexStatus status = reldex_workspace_build_connect_params(
            m_workspace.get(), request, reinterpret_cast<const std::uint8_t *>(id16.constData()),
            password);
    if (status != RELDEX_STATUS_OK) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return false;
    }
    m_testConnectBuildRequest = request;
    return true;
}

void ConnectionManager::handleConnectParamsBuilt(const ReldexWorkspaceReply &reply)
{
    if (reply.request != m_testConnectBuildRequest || !m_testConnectBusy) {
        return;
    }
    const QString idHex = idHexOf(idToBytes16(m_testConnectProfileId));
    if (reply.error != nullptr) {
        adoptError(reply.error, false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return;
    }

    ReldexConnectSummaryView view = reldex::makeConnectSummaryView();
    if (reldex_connect_summary_view(reply.connect, &view)) {
        const QString connectString =
                QString::fromUtf8(reinterpret_cast<const char *>(view.connect_string.ptr),
                                   static_cast<int>(view.connect_string.len));
        if (!connectString.isEmpty()) {
            m_testConnectSummary = connectString;
        } else {
            const QString host = QString::fromUtf8(reinterpret_cast<const char *>(view.host.ptr),
                                                     static_cast<int>(view.host.len));
            const QString service =
                    QString::fromUtf8(reinterpret_cast<const char *>(view.service.ptr),
                                       static_cast<int>(view.service.len));
            m_testConnectSummary = QStringLiteral("%1:%2/%3").arg(host).arg(view.port).arg(service);
        }
    }

    if (!m_bridge) {
        // Defensive only -- this runs from drain(), which only ever executes
        // while this object (Bridge's own child) is alive, so `m_bridge`
        // going null here should not be reachable. Kept anyway so "never
        // dereference a gone Bridge" holds everywhere this class touches it,
        // not just in the destructor (see m_bridge's own doc comment).
        adoptError(nullptr, false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return;
    }

    // ADR-0003: which driver a session opens against is a build-time choice,
    // and this build's hub can only ever open the mock driver (see the class
    // documentation's FFI-gap note) -- passing null asks for exactly that,
    // its default scenario.
    ReldexSessionId sessionId = 0;
    const quint64 openRequest = nextRequest();
    const ReldexStatus status =
            reldex_hub_open_session(m_bridge->hub(), nullptr, openRequest, &sessionId);
    if (status != RELDEX_STATUS_OK || sessionId == 0) {
        adoptError(reldex_last_error_take(), false);
        Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
        m_testConnectBusy = false;
        Q_EMIT testConnectStateChanged();
        return;
    }
    m_testConnectSessionId = sessionId;
    m_bridge->registerHubSink(sessionId, this);
    // The open reply is awaited in handleHubEvent(); no separate "awaiting
    // open" flag is needed beyond `m_testConnectSessionId != 0`.
}

void ConnectionManager::handleHubEvent(const ReldexEvent &raw, reldex::BatchHandle batch,
                                        reldex::ErrorHandle error)
{
    Q_UNUSED(batch); // test-connect never executes a statement; nothing to fetch
    if (!m_testConnectBusy || raw.session != m_testConnectSessionId) {
        return;
    }
    if (!m_bridge) {
        // Defensive only, same reasoning as handleConnectParamsBuilt() above
        // -- there is no hub left to ping/close on; just stop being busy.
        finishTestConnect();
        return;
    }
    const QString idHex = idHexOf(idToBytes16(m_testConnectProfileId));

    switch (raw.kind) {
    case RELDEX_EVENT_KIND_OPENED: {
        if (error) {
            adoptError(error.get(), false);
            Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, false);
            finishTestConnect();
            return;
        }
        m_testConnectPingRequest = nextRequest();
        reldex_session_ping(m_bridge->hub(), m_testConnectSessionId, m_testConnectPingRequest);
        return;
    }
    case RELDEX_EVENT_KIND_COMPLETED: {
        if (error) {
            // ADR-0007: a stored password refused at the database is an
            // authentication failure; the caller offers to update it rather
            // than retrying automatically. `isAuthFailureAgainstStoredPassword()`
            // is gated on `m_testConnectUsedStoredPassword` (set only when
            // `resolve_password` itself resolved `FromStore`, in
            // `handlePasswordResolved()`) -- an authentication failure on its
            // own does not mean a *stored* password was refused; it could
            // just as well be a typed-in password or one the profile never
            // had at all. See that function's own doc comment for why this
            // build's mock driver cannot exercise this end to end and how it
            // is tested instead.
            ReldexErrorView view = reldex::makeErrorView();
            const bool decoded = reldex_error_view(error.get(), &view);
            const bool authFailure = decoded
                    && isAuthFailureAgainstStoredPassword(view.kind, m_testConnectUsedStoredPassword);
            adoptError(error.get(), false);
            Q_EMIT testConnectFailed(idHex, m_errorMessageKey, m_errorMessage, authFailure);
            m_testConnectCloseRequest = nextRequest();
            reldex_session_close(m_bridge->hub(), m_testConnectSessionId, m_testConnectCloseRequest,
                                  RELDEX_CLOSE_DISPOSITION_NONE);
            return;
        }
        Q_EMIT testConnectSucceeded(idHex, m_testConnectSummary);
        m_testConnectCloseRequest = nextRequest();
        reldex_session_close(m_bridge->hub(), m_testConnectSessionId, m_testConnectCloseRequest,
                              RELDEX_CLOSE_DISPOSITION_NONE);
        return;
    }
    case RELDEX_EVENT_KIND_SESSION_CLOSED:
    case RELDEX_EVENT_KIND_TERMINAL:
        finishTestConnect();
        return;
    default:
        return;
    }
}

void ConnectionManager::finishTestConnect()
{
    // `m_bridge` (see its own doc comment): a QPointer, checked here for the
    // same reason as in the destructor -- this can run with Bridge already
    // gone.
    if (m_testConnectSessionId != 0 && m_bridge) {
        m_bridge->unregisterHubSink(m_testConnectSessionId);
    }
    m_testConnectSessionId = 0;
    m_testConnectBusy = false;
    Q_EMIT testConnectStateChanged();
}
