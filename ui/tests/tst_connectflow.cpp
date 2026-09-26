#include "AdapterTestSupport.h"

#include <QElapsedTimer>
#include <QSignalSpy>
#include <QTest>

#include <ConnectionManager.h>

using adapter_test::spinUntil;

namespace {

QVariantMap profileFields(const QString &name)
{
    return QVariantMap {
        { QStringLiteral("name"), name },
        { QStringLiteral("environment"), RELDEX_ENVIRONMENT_KIND_DEVELOPMENT },
        { QStringLiteral("environmentLabel"), QString() },
        { QStringLiteral("treatAsProduction"), false },
        { QStringLiteral("endpointKind"), RELDEX_ENDPOINT_KIND_HOST_PORT },
        { QStringLiteral("host"), QStringLiteral("db.example.invalid") },
        { QStringLiteral("port"), 1521 },
        { QStringLiteral("serviceTargetKind"), RELDEX_SERVICE_TARGET_KIND_SERVICE_NAME },
        { QStringLiteral("serviceNameOrSid"), QStringLiteral("ORCL") },
        { QStringLiteral("connectString"), QString() },
        { QStringLiteral("username"), QStringLiteral("app_owner") },
        { QStringLiteral("authKind"), RELDEX_AUTH_KIND_PASSWORD },
        { QStringLiteral("passwordStorage"), RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME },
        { QStringLiteral("password"), QString() },
        { QStringLiteral("sessionRole"), RELDEX_SESSION_ROLE_KIND_NORMAL },
        { QStringLiteral("transport"), RELDEX_TRANSPORT_KIND_PLAIN },
        { QStringLiteral("caDirectory"), QString() },
        { QStringLiteral("allowUnenforcedCertificatePin"), false },
    };
}

/// Not a latency assertion: every call below returns while a connect is
/// parked *indefinitely* by the mock, so a call that waited on it would never
/// return at all. The bound only turns that hang into a readable failure.
constexpr qint64 kReturnsAtOnceMs = 5000;

} // namespace

// M3.3: the worksheet connect flow in `SessionController` + `ConnectionManager`,
// driven through the real workspace (in-memory store, memory credential store)
// and the real prepare step, with the *mock* driver standing in for the
// database (`setConnectDriverForTesting`). The same flow against the real
// Oracle test database is `tst_coreinfo`'s `connectFlowAgainstTheRealDatabase`
// and `crates/ffi/tests/m3_3_connect_live.rs`.
class TstConnectFlow : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void init();
    void cleanup();

    void aPromptEachTimeProfileAsksForAPasswordThenConnects();
    void aConnectedWorksheetIsNeverSilentlyReplaced();
    void theProductionIndicatorFollowsTheConnectedSession();
    void aStoredPasswordConnectsWithoutAPrompt();
    void aRefusedStoredPasswordPromptsNextTimeAndIsNeverRetried();
    void aTypedPasswordIsSavedOnlyAfterSuccessAndOnlyWhenAsked();
    void externalAuthenticationNeedsNoPassword();
    void cancellingAConnectReturnsAtOnceAndAdoptsNothingLate();
    void cancellingWhileAskingForAPasswordLeavesNothingBehind();
    void aConnectPastTheSettingsLimitTimesOut();
    void aFailedConnectKeepsTheErrorKind();
    void aLostSessionLandsInTheUiStateAndAllowsANewConnect();
    void disconnectingWithAPossiblyOpenTransactionWaitsForADecision();

private:
    /// Opens the workspace, creates `fields` as a profile and returns its id.
    QString createProfile(const QVariantMap &fields);
    /// Sets `idHex`'s profile-level `CONNECT_TIMEOUT` through the workspace
    /// itself (there is no settings UI yet, M3.6).
    void setProfileConnectTimeout(const QString &idHex, quint32 seconds);
    /// Connects a prompt-each-time profile by typing a password.
    bool connectTyped(const QString &idHex, bool save = false);
    bool waitForConnectState(SessionController::ConnectState state);

    std::unique_ptr<Bridge> m_bridge;
    SessionController *m_session = nullptr;
    ConnectionManager *m_manager = nullptr;
    adapter_test::LiveCounts m_baseline;
};

void TstConnectFlow::init()
{
    m_baseline = adapter_test::settledBaseline();
    m_bridge = std::make_unique<Bridge>();
    QVERIFY(m_bridge->isValid());
    m_session = m_bridge->session();
    m_manager = m_bridge->connections();
    QVERIFY(m_manager->open());
    QVERIFY(spinUntil([this] { return m_manager->isReady(); }));
    m_session->setConnectDriverForTesting(RELDEX_DRIVER_KIND_MOCK);
    m_session->setMockRows(3);
}

void TstConnectFlow::cleanup()
{
    m_bridge.reset();
    m_session = nullptr;
    m_manager = nullptr;
    QVERIFY2(adapter_test::spinUntilLiveCounts(m_baseline),
             qPrintable(QStringLiteral("baseline %1, now %2")
                                .arg(m_baseline.toString(), adapter_test::liveCounts().toString())));
}

QString TstConnectFlow::createProfile(const QVariantMap &fields)
{
    QSignalSpy saved(m_manager, &ConnectionManager::profileSaved);
    if (!m_manager->createProfile(fields)) {
        return {};
    }
    if (!spinUntil([&saved] { return saved.count() >= 1; })) {
        return {};
    }
    return saved.constFirst().at(0).toString();
}

void TstConnectFlow::setProfileConnectTimeout(const QString &idHex, quint32 seconds)
{
    ReldexSettingValue value = reldex::sized<ReldexSettingValue>();
    value.kind = RELDEX_VALUE_KIND_TIME_LIMIT;
    value.number_value = seconds;
    const QByteArray id = QByteArray::fromHex(idHex.toLatin1());
    // Its reply is drained and ignored by the manager; the service thread
    // applies commands in order, so the prepare submitted after this sees it.
    QCOMPARE(reldex_workspace_set_setting(m_manager->m_workspace.get(), m_manager->nextRequest(),
                                          RELDEX_SETTING_ID_CONNECT_TIMEOUT,
                                          RELDEX_SETTING_LEVEL_PROFILE,
                                          reinterpret_cast<const std::uint8_t *>(id.constData()),
                                          &value),
             RELDEX_STATUS_OK);
}

bool TstConnectFlow::waitForConnectState(SessionController::ConnectState state)
{
    return spinUntil([this, state] { return m_session->connectState() == state; });
}

bool TstConnectFlow::connectTyped(const QString &idHex, bool save)
{
    if (!m_session->connectProfile(idHex)) {
        return false;
    }
    if (!waitForConnectState(SessionController::AwaitingPassword)) {
        return false;
    }
    return m_session->submitPassword(QStringLiteral("typed-pw-5b1e"), save);
}

void TstConnectFlow::aPromptEachTimeProfileAsksForAPasswordThenConnects()
{
    const QString id = createProfile(profileFields(QStringLiteral("Orders (dev)")));
    QVERIFY(!id.isEmpty());

    QSignalSpy states(m_session, &SessionController::connectStateChanged);
    QVERIFY(m_session->connectProfile(id));
    QCOMPARE(m_session->connectState(), SessionController::Preparing);
    QCOMPARE(m_session->connectProfileName(), QStringLiteral("Orders (dev)"));
    QCOMPARE(m_session->connectProfileId(), id);

    QVERIFY(waitForConnectState(SessionController::AwaitingPassword));
    QCOMPARE(m_session->passwordPromptReason(), SessionController::PromptEachTime);
    QVERIFY(m_session->offerSavePassword()); // the memory store can hold one
    QCOMPARE(m_session->sessionId(), quint64 { 0 }); // nothing opened yet

    QVERIFY(m_session->submitPassword(QStringLiteral("typed-pw-5b1e"), false));
    QCOMPARE(m_session->connectState(), SessionController::Preparing);
    QCOMPARE(m_session->passwordPromptReason(), SessionController::NoPrompt);

    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->sessionId() != 0);
    QCOMPARE(m_session->state(), SessionController::Ready);
    QVERIFY(!m_session->hasError());
    QVERIFY(m_session->lastConnectMs() >= 0);
    QCOMPARE(m_session->connectTimeoutSeconds(), 15); // the settings registry's default
    // Preparing -> AwaitingPassword -> Preparing -> Connecting -> Connected.
    QCOMPARE(states.count(), 5);

    QVERIFY(m_session->disconnectSession());
    QCOMPARE(m_session->connectState(), SessionController::Disconnecting);
    QVERIFY(waitForConnectState(SessionController::NotConnected));
}

void TstConnectFlow::aConnectedWorksheetIsNeverSilentlyReplaced()
{
    const QString first = createProfile(profileFields(QStringLiteral("First")));
    const QString second = createProfile(profileFields(QStringLiteral("Second")));
    QVERIFY(connectTyped(first));
    QVERIFY(waitForConnectState(SessionController::Connected));
    const quint64 session = m_session->sessionId();

    QVERIFY(!m_session->connectProfile(second));
    QCOMPARE(m_session->sessionId(), session);
    QCOMPARE(m_session->connectProfileName(), QStringLiteral("First"));
    QCOMPARE(m_session->connectState(), SessionController::Connected);

    // Nor while one is in flight.
    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::NotConnected));
    QVERIFY(m_session->connectProfile(second));
    QVERIFY(!m_session->connectProfile(first));
    QVERIFY(m_session->cancelConnect());
}

void TstConnectFlow::theProductionIndicatorFollowsTheConnectedSession()
{
    QVariantMap fields = profileFields(QStringLiteral("Orders (prod)"));
    fields["environment"] = RELDEX_ENVIRONMENT_KIND_PRODUCTION;
    fields["treatAsProduction"] = true;
    const QString id = createProfile(fields);
    QVERIFY(!id.isEmpty());

    QSignalSpy production(m_session, &SessionController::activeProfileIsProductionChanged);
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::AwaitingPassword));
    QVERIFY(!m_session->activeProfileIsProduction()); // not while only asking
    QVERIFY(m_session->submitPassword(QStringLiteral("typed-pw-5b1e"), false));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->activeProfileIsProduction());

    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::NotConnected));
    QVERIFY(!m_session->activeProfileIsProduction());
    QCOMPARE(production.count(), 2);

    // A failed connect never raises it.
    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_UNREACHABLE);
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::ConnectFailed));
    QVERIFY(!m_session->activeProfileIsProduction());
    QCOMPARE(production.count(), 2);
}

void TstConnectFlow::aStoredPasswordConnectsWithoutAPrompt()
{
    QVariantMap fields = profileFields(QStringLiteral("Stored"));
    fields["passwordStorage"] = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
    fields["password"] = QStringLiteral("stored-pw-77c2");
    const QString id = createProfile(fields);
    QVERIFY(!id.isEmpty());

    QSignalSpy prompts(m_session, &SessionController::passwordPromptChanged);
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->usedStoredPassword());
    QCOMPARE(prompts.count(), 0);
}

void TstConnectFlow::aRefusedStoredPasswordPromptsNextTimeAndIsNeverRetried()
{
    QVariantMap fields = profileFields(QStringLiteral("Stored, stale"));
    fields["passwordStorage"] = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
    fields["password"] = QStringLiteral("stale-pw-0a9d");
    const QString id = createProfile(fields);
    const QByteArray idBytes = QByteArray::fromHex(id.toLatin1());

    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_AUTHENTICATION);
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::ConnectFailed));
    QCOMPARE(m_session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_AUTHENTICATION));
    QVERIFY(m_session->usedStoredPassword());
    QVERIFY(m_manager->storedPasswordWasRefused(idBytes));

    // The next connect asks at once: the stored password is not tried again.
    QVERIFY(m_session->connectProfile(id));
    QCOMPARE(m_session->connectState(), SessionController::AwaitingPassword);
    QCOMPARE(m_session->passwordPromptReason(), SessionController::StoredPasswordRefused);
    QVERIFY(m_session->offerSavePassword()); // offered: update the saved one
    QCOMPARE(m_session->sessionId(), quint64 { 0 });

    // A typed password that works, with "update the saved password" ticked.
    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_NONE);
    QSignalSpy saved(m_manager, &ConnectionManager::profileSaved);
    QVERIFY(m_session->submitPassword(QStringLiteral("fresh-pw-4c11"), true));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(!m_session->usedStoredPassword());
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QVERIFY(!m_manager->storedPasswordWasRefused(idBytes));

    // And the store is trusted again.
    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::NotConnected));
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->usedStoredPassword());
}

void TstConnectFlow::aTypedPasswordIsSavedOnlyAfterSuccessAndOnlyWhenAsked()
{
    const QString id = createProfile(profileFields(QStringLiteral("Typed")));
    auto storage = [this, &id] {
        const ProfileModel *profiles = m_manager->profiles();
        return profiles->get(profiles->indexOfId(id)).value("passwordStorage").toInt();
    };
    QSignalSpy saved(m_manager, &ConnectionManager::profileSaved);

    // Asked to save, but the connect fails: nothing is saved.
    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_UNREACHABLE);
    QVERIFY(connectTyped(id, true));
    QVERIFY(waitForConnectState(SessionController::ConnectFailed));
    // Give a stray save every chance to arrive before saying it did not.
    QVERIFY(!spinUntil([&saved] { return saved.count() > 0; }, 500));
    QCOMPARE(storage(), static_cast<int>(RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME));

    // Succeeds, but the user did not ask: nothing is saved.
    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_NONE);
    QVERIFY(connectTyped(id, false));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(!spinUntil([&saved] { return saved.count() > 0; }, 500));
    QCOMPARE(storage(), static_cast<int>(RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME));
    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::NotConnected));

    // Succeeds and asked: put, then the flag (M3.2's write order).
    QVERIFY(connectTyped(id, true));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QCOMPARE(storage(), static_cast<int>(RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE));
    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::NotConnected));

    // The next connect needs no prompt.
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->usedStoredPassword());
}

void TstConnectFlow::externalAuthenticationNeedsNoPassword()
{
    QVariantMap fields = profileFields(QStringLiteral("External"));
    fields["authKind"] = RELDEX_AUTH_KIND_EXTERNAL;
    const QString id = createProfile(fields);
    QSignalSpy prompts(m_session, &SessionController::passwordPromptChanged);
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QCOMPARE(prompts.count(), 0);
    QVERIFY(!m_session->usedStoredPassword());
}

void TstConnectFlow::cancellingAConnectReturnsAtOnceAndAdoptsNothingLate()
{
    const QString id = createProfile(profileFields(QStringLiteral("Slow")));
    m_session->setMockBlockConnect(true); // parked until released: never answers alone

    QElapsedTimer call;
    call.start();
    QVERIFY(connectTyped(id));
    QVERIFY(call.elapsed() < kReturnsAtOnceMs);
    QVERIFY(waitForConnectState(SessionController::Connecting));
    const quint64 abandoned = m_session->sessionId();
    QVERIFY(abandoned != 0);
    QCOMPARE(m_session->state(), SessionController::Opening);

    call.restart();
    QVERIFY(m_session->cancelConnect());
    QVERIFY(call.elapsed() < kReturnsAtOnceMs);
    QCOMPARE(m_session->connectState(), SessionController::Cancelled);
    QCOMPARE(m_session->sessionId(), quint64 { 0 });
    QCOMPARE(m_session->state(), SessionController::Idle);
    QVERIFY(!m_session->hasError());

    // The connect now "arrives" late. It must be closed, never adopted: the
    // UI stays exactly as the cancel left it, and the session retires.
    const qint64 orphansBefore = m_bridge->orphanEvents();
    QVERIFY(m_bridge->releaseMockBlock(abandoned));
    QVERIFY(spinUntil([this] { return reldex_hub_session_count(m_bridge->hub()) == 0; }));
    QTest::qWait(50); // let any stray event reach the controller
    QCOMPARE(m_session->connectState(), SessionController::Cancelled);
    QCOMPARE(m_session->sessionId(), quint64 { 0 });
    QVERIFY(!m_session->hasError());
    QCOMPARE(m_bridge->orphanEvents(), orphansBefore); // consumed, not stray

    // And the worksheet can connect again.
    m_session->setMockBlockConnect(false);
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
}

void TstConnectFlow::cancellingWhileAskingForAPasswordLeavesNothingBehind()
{
    const QString id = createProfile(profileFields(QStringLiteral("Asked")));
    QVERIFY(m_session->connectProfile(id));
    QVERIFY(m_session->cancelConnect()); // while the workspace is preparing
    QCOMPARE(m_session->connectState(), SessionController::Cancelled);
    QTest::qWait(50); // the prepare's reply arrives and is dropped unread
    QCOMPARE(m_session->connectState(), SessionController::Cancelled);

    QVERIFY(m_session->connectProfile(id));
    QVERIFY(waitForConnectState(SessionController::AwaitingPassword));
    QVERIFY(m_session->cancelConnect());
    QCOMPARE(m_session->connectState(), SessionController::Cancelled);
    QCOMPARE(m_session->passwordPromptReason(), SessionController::NoPrompt);
    QVERIFY(!m_session->submitPassword(QStringLiteral("too-late"), false));
    QCOMPARE(m_session->sessionId(), quint64 { 0 });
}

void TstConnectFlow::aConnectPastTheSettingsLimitTimesOut()
{
    const QString id = createProfile(profileFields(QStringLiteral("Unreachable")));
    setProfileConnectTimeout(id, 1);
    m_session->setMockBlockConnect(true);

    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::Connecting));
    QCOMPARE(m_session->connectTimeoutSeconds(), 1); // from the settings registry
    const quint64 abandoned = m_session->sessionId();

    QVERIFY(waitForConnectState(SessionController::TimedOut));
    QCOMPARE(m_session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_TIMEOUT));
    QCOMPARE(m_session->sessionId(), quint64 { 0 });
    QVERIFY(abandoned != 0);

    // The abandoned session retires once its TERMINAL is drained, which also
    // releases the mock's parked connect: the connection that then "arrives"
    // is closed by the library, never adopted.
    QVERIFY(spinUntil([this] { return reldex_hub_session_count(m_bridge->hub()) == 0; }));
    QTest::qWait(50);
    QCOMPARE(m_session->connectState(), SessionController::TimedOut);
    QCOMPARE(m_session->sessionId(), quint64 { 0 });
}

void TstConnectFlow::aFailedConnectKeepsTheErrorKind()
{
    const QString id = createProfile(profileFields(QStringLiteral("Refused")));
    m_session->setMockConnectFailure(RELDEX_MOCK_FAILURE_UNREACHABLE);
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::ConnectFailed));
    QVERIFY(m_session->hasError());
    QCOMPARE(m_session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_CONNECTION));
    QVERIFY(!m_session->errorMessage().isEmpty());
    // The failed session retires on its own TERMINAL.
    QVERIFY(spinUntil([this] { return reldex_hub_session_count(m_bridge->hub()) == 0; }));
    QCOMPARE(m_session->connectState(), SessionController::ConnectFailed);
}

void TstConnectFlow::aLostSessionLandsInTheUiStateAndAllowsANewConnect()
{
    QVariantMap fields = profileFields(QStringLiteral("Flaky (prod)"));
    fields["environment"] = RELDEX_ENVIRONMENT_KIND_PRODUCTION;
    fields["treatAsProduction"] = true;
    const QString id = createProfile(fields);
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->activeProfileIsProduction());

    QVERIFY(m_session->executeMockStatement(RELDEX_MOCK_STATEMENT_LOSE_SESSION));
    QVERIFY(waitForConnectState(SessionController::Lost));
    QVERIFY(m_session->isTerminated());
    QVERIFY(m_session->hasError());
    QVERIFY(!m_session->activeProfileIsProduction());
    QCOMPARE(m_session->state(), SessionController::Failed);

    // The user reconnects explicitly: a new session, not a silent swap.
    const quint64 lostSession = m_session->sessionId();
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->sessionId() != lostSession);
    QVERIFY(!m_session->isTerminated());
}

void TstConnectFlow::disconnectingWithAPossiblyOpenTransactionWaitsForADecision()
{
    const QString id = createProfile(profileFields(QStringLiteral("Writer")));
    QVERIFY(connectTyped(id));
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->executeMockStatement(RELDEX_MOCK_STATEMENT_DML));
    QVERIFY(spinUntil([this] { return m_session->transactionPossiblyActive(); }));

    // No decision named: refused, still connected, and the error says why.
    QVERIFY(m_session->disconnectSession());
    QVERIFY(waitForConnectState(SessionController::Connected));
    QVERIFY(m_session->hasError());
    QCOMPARE(m_session->errorKind(), static_cast<int>(RELDEX_ERROR_KIND_TRANSACTION));

    // The user decides: roll back.
    QSignalSpy terminated(m_session, &SessionController::terminated);
    QVERIFY(m_session->disconnectSession(RELDEX_CLOSE_DISPOSITION_ROLLBACK));
    QVERIFY(waitForConnectState(SessionController::NotConnected));
    QCOMPARE(terminated.count(), 1);
    QCOMPARE(terminated.constFirst().at(0).toBool(), false); // nothing lost: it was decided
}

QTEST_GUILESS_MAIN(TstConnectFlow)

#include "tst_connectflow.moc"
