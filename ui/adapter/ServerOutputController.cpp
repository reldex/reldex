#include "ServerOutputController.h"

#include "Bridge.h"
#include "SessionController.h"

#include <algorithm>
#include <utility>

ServerOutputController::ServerOutputController(Bridge *bridge, SettingsController *settings,
                                               QByteArray worksheetId, QObject *parent)
    : QObject(parent), m_worksheetId(std::move(worksheetId)), m_settings(settings)
{
    m_model = new ServerOutputModel(this);
    m_session = new SessionController(bridge, this);
    // Without this, `reldex_session_set_server_output` is refused and
    // `RELDEX_MOCK_STATEMENT_SERVER_OUTPUT` prints nothing (see
    // `SessionController::mockServerOutputSupported`'s own doc comment).
    m_session->setMockServerOutputSupported(true);

    connect(m_session, &SessionController::opened, this,
            &ServerOutputController::handleSessionOpened);
    connect(m_session, &SessionController::serverOutputReceived, this,
            &ServerOutputController::handleSessionServerOutput);
    connect(m_session, &SessionController::serverOutputConfigureFailed, this, [] {
        qWarning("ServerOutputController: reldex_session_set_server_output was refused -- the "
                 "mock scenario's server_output capability may not be advertised");
    });

    if (m_settings) {
        connect(m_settings, &SettingsController::readyChanged, this,
                &ServerOutputController::ensureSettingsResolveRequested);
        connect(m_settings, &SettingsController::resolved, this,
                &ServerOutputController::handleResolved);
        connect(m_settings, &SettingsController::resolveFailed, this,
                &ServerOutputController::handleResolveFailed);
        connect(m_settings, &SettingsController::valueSet, this,
                &ServerOutputController::handleSet);
        connect(m_settings, &SettingsController::setFailed, this,
                &ServerOutputController::handleSetFailed);
        connect(m_settings, &SettingsController::valueCleared, this,
                &ServerOutputController::handleCleared);
        connect(m_settings, &SettingsController::clearFailed, this,
                &ServerOutputController::handleClearFailed);
        connect(m_settings, &SettingsController::worksheetEnsured, this,
                &ServerOutputController::handleWorksheetEnsured);
        connect(m_settings, &SettingsController::worksheetEnsureFailed, this,
                &ServerOutputController::handleWorksheetEnsureFailed);
    }
}

ServerOutputController::~ServerOutputController() = default;

bool ServerOutputController::open()
{
    m_openCalled = true;
    bool ok = true;
    if (m_settings) {
        ok = m_settings->open() && ok;
        ensureSettingsResolveRequested();
    } else {
        ok = false;
    }
    ok = m_session->open() && ok;
    return ok;
}

void ServerOutputController::ensureSettingsResolveRequested()
{
    if (m_resolveRequested || !m_settings || !m_settings->isReady()) {
        return;
    }
    m_resolveRequested = true;
    requestResolveAll();
}

void ServerOutputController::requestResolveAll()
{
    if (!m_settings) {
        return;
    }
    const quint64 enabledRequest =
            m_settings->resolve(RELDEX_SETTING_ID_SERVER_OUTPUT_ENABLED, QByteArray(), m_worksheetId);
    if (enabledRequest != 0) {
        m_ownRequests.insert(enabledRequest);
    }
    const quint64 bufferRequest =
            m_settings->resolve(RELDEX_SETTING_ID_SERVER_OUTPUT_BUFFER, QByteArray(), m_worksheetId);
    if (bufferRequest != 0) {
        m_ownRequests.insert(bufferRequest);
    }
    // Must land before `ready()` is true (`maybeArmAndUpdateReady()`), not
    // just before this pane's first write: `worksheet_setting`'s foreign key
    // on `worksheet` means `setEnabled`/`setBufferBytes`/etc. fail silently
    // (`setFailed`, never `valueSet`) for a worksheet id that was only ever
    // generated, never saved. Reading (the two resolves above) is unaffected.
    const quint64 ensureRequest = m_settings->ensureWorksheet(m_worksheetId);
    if (ensureRequest != 0) {
        m_ownRequests.insert(ensureRequest);
    }
}

void ServerOutputController::handleResolved(quint64 request, int settingId,
                                            SettingsController::Value value, int level)
{
    if (!m_ownRequests.remove(request)) {
        return;
    }
    if (settingId == RELDEX_SETTING_ID_SERVER_OUTPUT_ENABLED) {
        m_enabled = value.boolValue;
        m_enabledLevel = level;
        m_enabledSeen = true;
        Q_EMIT enabledChanged();
    } else if (settingId == RELDEX_SETTING_ID_SERVER_OUTPUT_BUFFER) {
        m_unlimited = value.noLimit;
        m_bufferBytes = value.numberValue;
        m_bufferLevel = level;
        m_bufferSeen = true;
        Q_EMIT bufferChanged();
    }
    maybeArmAndUpdateReady();
}

void ServerOutputController::handleResolveFailed(quint64 request, int settingId,
                                                 const QString &message)
{
    if (request != 0 && !m_ownRequests.remove(request)) {
        return;
    }
    Q_UNUSED(settingId);
    Q_EMIT settingsError(message);
}

void ServerOutputController::handleSet(quint64 request, int settingId)
{
    if (!m_ownRequests.remove(request) || !m_settings) {
        return;
    }
    // Round-trip: read the value back so `enabledLevel`/`bufferLevel` report
    // `LevelWorksheet` from the store itself, not assumed locally.
    const quint64 resolveRequest =
            m_settings->resolve(settingId, QByteArray(), m_worksheetId);
    if (resolveRequest != 0) {
        m_ownRequests.insert(resolveRequest);
    }
}

void ServerOutputController::handleSetFailed(quint64 request, int settingId, const QString &message)
{
    if (request != 0 && !m_ownRequests.remove(request)) {
        return;
    }
    Q_UNUSED(settingId);
    Q_EMIT settingsError(message);
}

void ServerOutputController::handleCleared(quint64 request, int settingId)
{
    if (!m_ownRequests.remove(request) || !m_settings) {
        return;
    }
    const quint64 resolveRequest =
            m_settings->resolve(settingId, QByteArray(), m_worksheetId);
    if (resolveRequest != 0) {
        m_ownRequests.insert(resolveRequest);
    }
}

void ServerOutputController::handleClearFailed(quint64 request, int settingId,
                                               const QString &message)
{
    if (request != 0 && !m_ownRequests.remove(request)) {
        return;
    }
    Q_UNUSED(settingId);
    Q_EMIT settingsError(message);
}

void ServerOutputController::handleWorksheetEnsured(quint64 request)
{
    if (!m_ownRequests.remove(request)) {
        return;
    }
    m_worksheetEnsured = true;
    maybeArmAndUpdateReady();
}

void ServerOutputController::handleWorksheetEnsureFailed(quint64 request, const QString &message)
{
    if (request != 0 && !m_ownRequests.remove(request)) {
        return;
    }
    Q_EMIT settingsError(message);
}

void ServerOutputController::handleSessionOpened()
{
    m_sessionReady = true;
    maybeArmAndUpdateReady();
}

void ServerOutputController::maybeArmAndUpdateReady()
{
    const bool everythingKnown =
            m_sessionReady && m_enabledSeen && m_bufferSeen && m_worksheetEnsured;
    if (everythingKnown) {
        armSession();
    }
    if (everythingKnown != m_ready) {
        m_ready = everythingKnown;
        Q_EMIT readyChanged();
    }
}

void ServerOutputController::armSession()
{
    const int mode = !m_enabled
            ? RELDEX_SERVER_OUTPUT_MODE_DISABLED
            : (m_unlimited ? RELDEX_SERVER_OUTPUT_MODE_ENABLED_UNLIMITED
                            : RELDEX_SERVER_OUTPUT_MODE_ENABLED_BYTES);
    m_session->setServerOutput(mode, m_unlimited ? 0 : m_bufferBytes);
}

bool ServerOutputController::setEnabled(bool enabled)
{
    if (!m_settings) {
        return false;
    }
    const quint64 request =
            m_settings->setValue(RELDEX_SETTING_ID_SERVER_OUTPUT_ENABLED,
                                 RELDEX_SETTING_LEVEL_WORKSHEET, m_worksheetId,
                                 SettingsController::Value::ofBool(enabled));
    if (request == 0) {
        return false;
    }
    m_ownRequests.insert(request);
    return true;
}

bool ServerOutputController::setBufferBytes(quint32 bytes)
{
    if (!m_settings) {
        return false;
    }
    const quint32 clamped = std::min(kMaxBufferBytes, std::max(kMinBufferBytes, bytes));
    const quint64 request =
            m_settings->setValue(RELDEX_SETTING_ID_SERVER_OUTPUT_BUFFER,
                                 RELDEX_SETTING_LEVEL_WORKSHEET, m_worksheetId,
                                 SettingsController::Value::ofByteLimitBytes(clamped));
    if (request == 0) {
        return false;
    }
    m_ownRequests.insert(request);
    return true;
}

bool ServerOutputController::setUnlimited()
{
    if (!m_settings) {
        return false;
    }
    const quint64 request =
            m_settings->setValue(RELDEX_SETTING_ID_SERVER_OUTPUT_BUFFER,
                                 RELDEX_SETTING_LEVEL_WORKSHEET, m_worksheetId,
                                 SettingsController::Value::ofByteLimitUnlimited());
    if (request == 0) {
        return false;
    }
    m_ownRequests.insert(request);
    return true;
}

bool ServerOutputController::clearEnabledOverride()
{
    if (!m_settings) {
        return false;
    }
    const quint64 request = m_settings->clearValue(RELDEX_SETTING_ID_SERVER_OUTPUT_ENABLED,
                                                    RELDEX_SETTING_LEVEL_WORKSHEET, m_worksheetId);
    if (request == 0) {
        return false;
    }
    m_ownRequests.insert(request);
    return true;
}

bool ServerOutputController::clearBufferOverride()
{
    if (!m_settings) {
        return false;
    }
    const quint64 request = m_settings->clearValue(RELDEX_SETTING_ID_SERVER_OUTPUT_BUFFER,
                                                    RELDEX_SETTING_LEVEL_WORKSHEET, m_worksheetId);
    if (request == 0) {
        return false;
    }
    m_ownRequests.insert(request);
    return true;
}

void ServerOutputController::clear()
{
    m_model->clear();
    if (m_droppedLines != 0 || m_invalidUtf8Lines != 0 || m_readFailed) {
        m_droppedLines = 0;
        m_invalidUtf8Lines = 0;
        m_readFailed = false;
        m_readFailureMessage.clear();
        Q_EMIT truncationChanged();
    }
}

bool ServerOutputController::runSampleStatement()
{
    return m_session->executeMockStatement(RELDEX_MOCK_STATEMENT_SERVER_OUTPUT);
}

void ServerOutputController::handleSessionServerOutput(const QStringList &lines, quint32 dropped,
                                                       quint32 invalidUtf8Lines, bool readFailed,
                                                       const QString &errorMessage)
{
    if (!lines.isEmpty()) {
        m_model->appendLines(lines);
    }
    bool changed = false;
    if (dropped > 0) {
        m_droppedLines += dropped;
        changed = true;
    }
    if (invalidUtf8Lines > 0) {
        m_invalidUtf8Lines += invalidUtf8Lines;
        changed = true;
    }
    if (readFailed) {
        m_readFailed = true;
        m_readFailureMessage = errorMessage;
        changed = true;
    }
    if (changed) {
        Q_EMIT truncationChanged();
    }
}
