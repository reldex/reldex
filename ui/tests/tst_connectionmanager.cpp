#include "AdapterTestSupport.h"

#include <QFileInfo>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QTest>

#include <cstdint>
#include <memory>

using adapter_test::spinUntil;

namespace {

// Sets an environment variable for the scope of one test, restoring whatever
// was there before -- robust against a `QVERIFY` failure returning early out
// of the middle of a test function, unlike plain qputenv/qunsetenv calls at
// the top and bottom of one.
class EnvGuard
{
public:
    EnvGuard(const char *name, const QByteArray &value) : m_name(name)
    {
        m_hadPrevious = qEnvironmentVariableIsSet(name);
        if (m_hadPrevious) {
            m_previous = qgetenv(name);
        }
        qputenv(name, value);
    }
    ~EnvGuard()
    {
        if (m_hadPrevious) {
            qputenv(m_name, m_previous);
        } else {
            qunsetenv(m_name);
        }
    }
    EnvGuard(const EnvGuard &) = delete;
    EnvGuard &operator=(const EnvGuard &) = delete;

private:
    const char *m_name;
    bool m_hadPrevious = false;
    QByteArray m_previous;
};

/// The env var `ConnectionManager::resolveDefaultStorePath()` reads on this
/// platform (ADR-0006 P5) -- mirrored here so the round-trip test exercises
/// the real, current-OS branch of that function rather than guessing one.
constexpr const char *kPlatformDataDirEnvVar =
#if defined(Q_OS_WIN)
        "LOCALAPPDATA";
#elif defined(Q_OS_MACOS)
        "HOME";
#else
        "XDG_DATA_HOME";
#endif

QVariantMap baseProfileFields()
{
    return QVariantMap {
        { QStringLiteral("name"), QStringLiteral("Test profile") },
        { QStringLiteral("environment"), RELDEX_ENVIRONMENT_KIND_DEVELOPMENT },
        { QStringLiteral("environmentLabel"), QString() },
        { QStringLiteral("treatAsProduction"), false },
        { QStringLiteral("endpointKind"), RELDEX_ENDPOINT_KIND_HOST_PORT },
        { QStringLiteral("host"), QStringLiteral("dbhost.example") },
        { QStringLiteral("port"), 1521 },
        { QStringLiteral("serviceTargetKind"), RELDEX_SERVICE_TARGET_KIND_SERVICE_NAME },
        { QStringLiteral("serviceNameOrSid"), QStringLiteral("ORCL") },
        { QStringLiteral("connectString"), QString() },
        { QStringLiteral("username"), QStringLiteral("scott") },
        { QStringLiteral("authKind"), RELDEX_AUTH_KIND_PASSWORD },
        { QStringLiteral("passwordStorage"), RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME },
        { QStringLiteral("password"), QString() },
        { QStringLiteral("sessionRole"), RELDEX_SESSION_ROLE_KIND_NORMAL },
        { QStringLiteral("transport"), RELDEX_TRANSPORT_KIND_PLAIN },
        { QStringLiteral("caDirectory"), QString() },
        { QStringLiteral("allowUnenforcedCertificatePin"), false },
    };
}

ReldexErrorView errorView(int32_t kind, bool hasNative = false, int32_t nativeCode = 0)
{
    ReldexErrorView view = reldex::makeErrorView();
    view.kind = kind;
    view.has_native = hasNative;
    view.native_code = nativeCode;
    return view;
}

} // namespace

// M3.2: `ConnectionManager` -- the connection-manager UI's adapter (list,
// create/edit/delete, test-connect). Every test opens its own in-memory (or,
// for the one path-resolution test, temp-file-backed) workspace through
// `Bridge::connections()`, never the developer's real store or Windows
// Credential Manager (`RELDEX_WORKSPACE_IN_MEMORY`/
// `RELDEX_WORKSPACE_MEMORY_CREDENTIAL_STORE`, read once at construction --
// see `ConnectionManager::open()`).
class TstConnectionManager : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void init();

    void opensInMemoryAndBecomesReady();
    void createListsUpdatesAndDeletesRoundTripThroughTheModel();
    void aPastedPasswordEqualsDescriptorIsRefusedWithoutEchoingIt();
    void environmentAndProductionFlagRulesAreEnforced();
    void aControlCharacterPasswordIsRefusedByTheCredentialStoreAndReportedAsAWarning();
    void testConnectSucceedsAgainstTheMockDriver();
    void testConnectFailsTypedWhenAPasswordIsRequiredAndNoneWasGiven();
    void testConnectResolvesAndUsesAStoredPasswordWithoutRetypingIt();
    void destroyingBridgeWithATestConnectInFlightDoesNotCrash();
    void authFailureAgainstStoredPasswordDecisionCoversAllFourCases();
    void messageKeyForErrorCoversEveryFfiErrorKind();
    void messageKeyForErrorCoversEveryCredentialErrorCode();
    void defaultStorePathMirrorsAdr0006PlatformRules();
    void aRealOnDiskWorkspaceRoundTripsThroughTheResolvedDefaultPath();

private:
    /// Every test that opens a real `ConnectionManager` wants the same two
    /// guards up before `Bridge` (and therefore `ConnectionManager`) is
    /// constructed.
    std::unique_ptr<EnvGuard> m_inMemory;
    std::unique_ptr<EnvGuard> m_memoryCredentials;
};

void TstConnectionManager::init()
{
    // Destroy the previous test's guards *before* constructing this test's --
    // not via plain unique_ptr assignment. `unique_ptr::operator=` builds the
    // new value, then calls reset(), whose own order is "assign the new
    // pointer, THEN delete the old one" -- so a bare
    // `m_inMemory = std::make_unique<EnvGuard>(...)` sets the env var to "1"
    // and only *afterwards* runs the previous guard's destructor, which
    // restores/unsets it again, undoing the set. That made every other test
    // in this file silently fall through to the real on-disk default store
    // (`ConnectionManager::open()` itself was never at fault -- this
    // was purely a test-harness ordering bug, found by instrumenting it and
    // seeing `forceInMemory` alternate 1/0/1/0 across declared test order).
    m_inMemory.reset();
    m_memoryCredentials.reset();
    m_inMemory = std::make_unique<EnvGuard>("RELDEX_WORKSPACE_IN_MEMORY", QByteArrayLiteral("1"));
    m_memoryCredentials = std::make_unique<EnvGuard>("RELDEX_WORKSPACE_MEMORY_CREDENTIAL_STORE",
                                                      QByteArrayLiteral("1"));
    qunsetenv("RELDEX_WORKSPACE_PATH");
}

void TstConnectionManager::opensInMemoryAndBecomesReady()
{
    Bridge bridge;
    QVERIFY(bridge.isValid());
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm != nullptr);
    // Must-fix 1: never open automatically. `Bridge`'s constructor only
    // allocates this object; nothing has called open() yet, so there is no
    // workspace handle and no I/O has happened anywhere.
    QVERIFY(!cm->isValid());

    QSignalSpy valid(cm, &ConnectionManager::validChanged);
    QSignalSpy opened(cm, &ConnectionManager::openFinished);
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));
    QVERIFY(cm->isValid());
    QVERIFY(valid.count() >= 1);
    QCOMPARE(opened.count(), 1);
    QCOMPARE(opened.constFirst().at(0).toBool(), true);
    QCOMPARE(cm->credentialStoreKind(), static_cast<int>(RELDEX_CREDENTIAL_STORE_KIND_MEMORY));
    QVERIFY(cm->canStoreCredential());
    QCOMPARE(cm->profiles()->rowCount(), 0);

    // Idempotent: a second call while already open is a harmless no-op.
    QVERIFY(cm->open());
}

void TstConnectionManager::createListsUpdatesAndDeletesRoundTripThroughTheModel()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(baseProfileFields()));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QCOMPARE(cm->profiles()->rowCount(), 1);
    const QString idHex = saved.constFirst().at(0).toString();
    QVERIFY(!idHex.isEmpty());
    QCOMPARE(saved.constFirst().at(1).toBool(), true); // created

    const int row = cm->profiles()->indexOfId(idHex);
    QVERIFY(row >= 0);
    QVariantMap fetched = cm->profiles()->get(row);
    QCOMPARE(fetched.value("name").toString(), QStringLiteral("Test profile"));
    QCOMPARE(fetched.value("host").toString(), QStringLiteral("dbhost.example"));
    QVERIFY(!fetched.contains("password")); // never a password field, by construction

    saved.clear();
    QVariantMap updated = baseProfileFields();
    updated["name"] = QStringLiteral("Renamed profile");
    QVERIFY(cm->updateProfile(idHex, updated));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QCOMPARE(saved.constFirst().at(1).toBool(), false); // not "created" this time
    QCOMPARE(cm->profiles()->rowCount(), 1);
    QCOMPARE(cm->profiles()->get(cm->profiles()->indexOfId(idHex)).value("name").toString(),
             QStringLiteral("Renamed profile"));

    QSignalSpy deleted(cm, &ConnectionManager::profileDeleted);
    QVERIFY(cm->deleteProfile(idHex));
    QVERIFY(spinUntil([&deleted] { return deleted.count() >= 1; }));
    QCOMPARE(deleted.constFirst().at(0).toString(), idHex);
    QCOMPARE(cm->profiles()->rowCount(), 0); // delete cascades into the UI model
}

void TstConnectionManager::aPastedPasswordEqualsDescriptorIsRefusedWithoutEchoingIt()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    const QString secret = QStringLiteral("Sup3rSecr3tPassword");
    QVariantMap fields = baseProfileFields();
    fields["endpointKind"] = RELDEX_ENDPOINT_KIND_CONNECT_STRING;
    fields["connectString"] = QStringLiteral("scott/%1@dbhost:1521/ORCL").arg(secret);

    QSignalSpy failed(cm, &ConnectionManager::profileSaveFailed);
    cm->createProfile(fields);
    QVERIFY(spinUntil([&failed] { return failed.count() >= 1; }));
    QCOMPARE(cm->profiles()->rowCount(), 0); // refused save leaves no trace

    const QString messageKey = failed.constFirst().at(0).toString();
    const QString message = failed.constFirst().at(1).toString();
    QCOMPARE(messageKey, QStringLiteral("error.configuration"));
    QVERIFY(message.contains(QStringLiteral("UserPasswordPrefix"))); // the pattern class...
    QVERIFY(!message.contains(secret)); // ...never the text
    QVERIFY(message.contains(QStringLiteral("connect string"))); // and names the field
}

void TstConnectionManager::environmentAndProductionFlagRulesAreEnforced()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    // Production without the flag: refused (ADR-0006 P3).
    {
        QSignalSpy failed(cm, &ConnectionManager::profileSaveFailed);
        QVariantMap fields = baseProfileFields();
        fields["environment"] = RELDEX_ENVIRONMENT_KIND_PRODUCTION;
        fields["treatAsProduction"] = false;
        cm->createProfile(fields);
        QVERIFY(spinUntil([&failed] { return failed.count() >= 1; }));
        QCOMPARE(failed.constFirst().at(0).toString(), QStringLiteral("error.configuration"));
    }

    // Production with the flag: accepted.
    {
        QSignalSpy saved(cm, &ConnectionManager::profileSaved);
        QVariantMap fields = baseProfileFields();
        fields["name"] = QStringLiteral("Prod profile");
        fields["environment"] = RELDEX_ENVIRONMENT_KIND_PRODUCTION;
        fields["treatAsProduction"] = true;
        QVERIFY(cm->createProfile(fields));
        QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
        const int row = cm->profiles()->indexOfId(saved.constFirst().at(0).toString());
        QVERIFY(cm->profiles()->get(row).value("treatAsProduction").toBool());
    }

    // A named, non-production environment with the flag on: refused.
    {
        QSignalSpy failed(cm, &ConnectionManager::profileSaveFailed);
        QVariantMap fields = baseProfileFields();
        fields["name"] = QStringLiteral("Bad staging profile");
        fields["environment"] = RELDEX_ENVIRONMENT_KIND_STAGING;
        fields["treatAsProduction"] = true;
        cm->createProfile(fields);
        QVERIFY(spinUntil([&failed] { return failed.count() >= 1; }));
    }

    // Custom is free to choose either way.
    {
        QSignalSpy saved(cm, &ConnectionManager::profileSaved);
        QVariantMap fields = baseProfileFields();
        fields["name"] = QStringLiteral("Custom profile");
        fields["environment"] = RELDEX_ENVIRONMENT_KIND_CUSTOM;
        fields["environmentLabel"] = QStringLiteral("Disaster recovery");
        fields["treatAsProduction"] = true;
        QVERIFY(cm->createProfile(fields));
        QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    }
}

void TstConnectionManager::aControlCharacterPasswordIsRefusedByTheCredentialStoreAndReportedAsAWarning()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));
    QVERIFY(cm->canStoreCredential()); // the in-memory test store (ADR-0007 S3)

    QVariantMap fields = baseProfileFields();
    fields["passwordStorage"] = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
    // A control character: refused by the credential store itself
    // (`CredentialError::InvalidSecret`, ADR-0007 S8), which the in-memory
    // test store enforces identically to the real backend.
    fields["password"] = QStringLiteral("bad\x01password");

    QSignalSpy warning(cm, &ConnectionManager::credentialWarning);
    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(fields));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QVERIFY(warning.count() >= 1); // "password not saved" (ADR-0007 write order)
    QCOMPARE(warning.constFirst().at(0).toString(), QStringLiteral("error.credential.invalidSecret"));

    // Write order: `put` failed, so the profile still carries prompt-each-time.
    const QString idHex = saved.constFirst().at(0).toString();
    const int row = cm->profiles()->indexOfId(idHex);
    QCOMPARE(cm->profiles()->get(row).value("passwordStorage").toInt(),
             static_cast<int>(RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME));
}

void TstConnectionManager::testConnectSucceedsAgainstTheMockDriver()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(baseProfileFields()));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    const QString idHex = saved.constFirst().at(0).toString();

    QSignalSpy succeeded(cm, &ConnectionManager::testConnectSucceeded);
    QSignalSpy failed(cm, &ConnectionManager::testConnectFailed);
    QVERIFY(cm->testConnect(idHex, QStringLiteral("whatever-the-user-typed")));
    QVERIFY(cm->testConnectBusy());
    QVERIFY(spinUntil([&succeeded, &failed] { return succeeded.count() + failed.count() >= 1; }));
    QCOMPARE(failed.count(), 0);
    QCOMPARE(succeeded.count(), 1);
    QCOMPARE(succeeded.constFirst().at(0).toString(), idHex);
    QVERIFY(!succeeded.constFirst().at(1).toString().isEmpty()); // a connect summary
    QVERIFY(spinUntil([cm] { return !cm->testConnectBusy(); }));
    // must-fix 4: an explicitly typed password never counts as "used the
    // store", even though it succeeded.
    QVERIFY(!cm->testConnectUsedStoredPassword());
}

void TstConnectionManager::testConnectFailsTypedWhenAPasswordIsRequiredAndNoneWasGiven()
{
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(baseProfileFields())); // Password auth, prompt-each-time
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    const QString idHex = saved.constFirst().at(0).toString();

    QSignalSpy failed(cm, &ConnectionManager::testConnectFailed);
    QSignalSpy succeeded(cm, &ConnectionManager::testConnectSucceeded);
    QVERIFY(cm->testConnect(idHex, QString())); // no password typed
    QVERIFY(spinUntil([&succeeded, &failed] { return succeeded.count() + failed.count() >= 1; }));
    QCOMPARE(succeeded.count(), 0);
    QCOMPARE(failed.count(), 1);
    QCOMPARE(failed.constFirst().at(1).toString(), QStringLiteral("error.configuration"));
    QCOMPARE(failed.constFirst().at(3).toBool(), false); // not an auth-with-stored-password failure
    QVERIFY(!cm->testConnectBusy());
}

void TstConnectionManager::testConnectResolvesAndUsesAStoredPasswordWithoutRetypingIt()
{
    // must-fix 4: testConnect() with no typed password asks resolve_password
    // rather than failing outright or guessing from the profile's own
    // passwordStorage flag -- a profile actually using the credential store
    // is test-connectable without re-typing its password.
    Bridge bridge;
    ConnectionManager *cm = bridge.connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));
    QVERIFY(cm->canStoreCredential()); // the in-memory test store (ADR-0007 S3)

    QVariantMap fields = baseProfileFields();
    fields["passwordStorage"] = RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE;
    fields["password"] = QStringLiteral("Sup3rSecr3tPassword"); // no control character: accepted

    QSignalSpy warning(cm, &ConnectionManager::credentialWarning);
    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(fields));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    QVERIFY(warning.isEmpty()); // the put succeeded; nothing to warn about
    const QString idHex = saved.constFirst().at(0).toString();
    QCOMPARE(cm->profiles()->get(cm->profiles()->indexOfId(idHex)).value("passwordStorage").toInt(),
             static_cast<int>(RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE));

    QSignalSpy succeeded(cm, &ConnectionManager::testConnectSucceeded);
    QSignalSpy failed(cm, &ConnectionManager::testConnectFailed);
    QVERIFY(cm->testConnect(idHex, QString())); // no password typed
    QVERIFY(spinUntil([&succeeded, &failed] { return succeeded.count() + failed.count() >= 1; }));
    QCOMPARE(failed.count(), 0);
    QCOMPARE(succeeded.count(), 1);
    // The whole point: resolve_password found it in the store and testConnect
    // used it, without the caller ever having to type it again.
    QVERIFY(cm->testConnectUsedStoredPassword());
    // Deliberately not spinning any further here: `bridge` goes out of scope
    // next with the session-close/finishTestConnect cycle not yet finished
    // (`m_testConnectSessionId` still non-zero, a hub sink still registered)
    // -- exactly the "destroy while Test Connect is busy" case
    // destroyingBridgeWithATestConnectInFlightDoesNotCrash() below exists to
    // prove safe. That dedicated test is the actual regression test for it;
    // this one stays focused on resolve_password.
}

void TstConnectionManager::destroyingBridgeWithATestConnectInFlightDoesNotCrash()
{
    // Round-2 review regression (2026-09-26): nothing stops the user closing
    // the app while Test Connect is busy. Before the fix, `~Bridge()` left
    // `ConnectionManager` -- its own QObject child -- to
    // `QObjectPrivate::deleteChildren()`'s automatic cleanup, which runs only
    // *after* `Bridge`'s own data members (including `m_hubSinks`) have
    // already been destructed; `~ConnectionManager()` still calling
    // `Bridge::unregisterHubSink()` for a test-connect session that had not
    // finished closing dereferenced that already-destructed `QHash`.
    // Reproduced 5/5 as an access violation on MSVC with the fix reverted;
    // this test crashed every time before `Bridge::~Bridge()` started
    // deleting `m_connections` explicitly (and `ConnectionManager::m_bridge`
    // became a `QPointer`, a second, independent line of defence) and passes
    // deterministically after.
    auto *bridge = new Bridge();
    ConnectionManager *cm = bridge->connections();
    QVERIFY(cm->open());
    QVERIFY(spinUntil([cm] { return cm->isReady(); }));

    QSignalSpy saved(cm, &ConnectionManager::profileSaved);
    QVERIFY(cm->createProfile(baseProfileFields()));
    QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    const QString idHex = saved.constFirst().at(0).toString();

    QSignalSpy succeeded(cm, &ConnectionManager::testConnectSucceeded);
    QSignalSpy failed(cm, &ConnectionManager::testConnectFailed);
    // A typed password, like testConnectSucceedsAgainstTheMockDriver(): no
    // resolve_password round trip, so the mock's open/ping/complete sequence
    // is the only thing standing between here and a registered hub sink.
    QVERIFY(cm->testConnect(idHex, QStringLiteral("whatever-the-user-typed")));
    QVERIFY(spinUntil([&succeeded, &failed] { return succeeded.count() + failed.count() >= 1; }));
    QCOMPARE(failed.count(), 0);
    QCOMPARE(succeeded.count(), 1);
    // The point of the whole test: test-connect succeeded, but the
    // session-close/finishTestConnect cycle it triggers has not run yet, so
    // `cm` is still busy and its hub sink is still registered with `bridge`
    // -- deliberately not waiting for `!cm->testConnectBusy()` here, unlike
    // every other test-connect test in this file.
    QVERIFY(cm->testConnectBusy());

    delete bridge; // must not crash (this is the entire assertion)
}

void TstConnectionManager::authFailureAgainstStoredPasswordDecisionCoversAllFourCases()
{
    // must-fix 4's decision, tested as the pure function it is refactored
    // into (`ConnectionManager::isAuthFailureAgainstStoredPassword()`'s own
    // doc comment explains why a real "fake hub event" through the private
    // method it is used from is not buildable in this build: crates/ffi
    // exposes no way to fabricate a ReldexError*). Only "authentication
    // failure" AND "used a stored password" together may offer to update the
    // stored password; either alone must not.
    QVERIFY(ConnectionManager::isAuthFailureAgainstStoredPassword(RELDEX_ERROR_KIND_AUTHENTICATION,
                                                                   true));
    QVERIFY(!ConnectionManager::isAuthFailureAgainstStoredPassword(RELDEX_ERROR_KIND_AUTHENTICATION,
                                                                    false));
    QVERIFY(!ConnectionManager::isAuthFailureAgainstStoredPassword(RELDEX_ERROR_KIND_CONNECTION, true));
    QVERIFY(
            !ConnectionManager::isAuthFailureAgainstStoredPassword(RELDEX_ERROR_KIND_CONNECTION, false));
}

void TstConnectionManager::messageKeyForErrorCoversEveryFfiErrorKind()
{
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_CONFIGURATION), false),
             QStringLiteral("error.configuration"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_CONNECTION), false),
             QStringLiteral("error.connection"));
    // The decision `handleHubEvent()` uses for ADR-0007's
    // "authFailureWithStoredPassword": this is the one FFI error kind that
    // ever sets it, unreachable through this build's mock driver (see
    // ConnectionManager.h), covered directly here instead.
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_AUTHENTICATION), false),
             QStringLiteral("error.authentication"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_NETWORK_LOST), false),
             QStringLiteral("error.networkLost"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_TIMEOUT), false),
             QStringLiteral("error.timeout"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_CANCELLED), false),
             QStringLiteral("error.cancelled"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_SYNTAX), false),
             QStringLiteral("error.syntax"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_CONSTRAINT), false),
             QStringLiteral("error.constraint"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_PERMISSION), false),
             QStringLiteral("error.permission"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_TRANSACTION), false),
             QStringLiteral("error.transaction"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_RESOURCE), false),
             QStringLiteral("error.resource"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_DATA_CONVERSION), false),
             QStringLiteral("error.dataConversion"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_UNSUPPORTED), false),
             QStringLiteral("error.unsupported"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_DRIVER_INTERNAL), false),
             QStringLiteral("error.driverInternal"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_OTHER), false),
             QStringLiteral("error.other"));
    QCOMPARE(ConnectionManager::messageKeyForError(errorView(RELDEX_ERROR_KIND_UNKNOWN), false),
             QStringLiteral("error.unknown"));
}

void TstConnectionManager::messageKeyForErrorCoversEveryCredentialErrorCode()
{
    auto key = [](int32_t code) {
        return ConnectionManager::messageKeyForError(
                errorView(RELDEX_ERROR_KIND_OTHER, true, code), true);
    };
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_UNAVAILABLE), QStringLiteral("error.credential.unavailable"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_NOT_FOUND), QStringLiteral("error.credential.notFound"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_DENIED), QStringLiteral("error.credential.denied"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_TOO_LARGE), QStringLiteral("error.credential.tooLarge"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_INVALID_SECRET),
             QStringLiteral("error.credential.invalidSecret"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_MALFORMED), QStringLiteral("error.credential.malformed"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_LOCKED), QStringLiteral("error.credential.locked"));
    QCOMPARE(key(RELDEX_CREDENTIAL_ERROR_BACKEND), QStringLiteral("error.credential.backend"));
    // Without `credentialContext`, the same view falls back to the plain
    // `ReldexErrorKind` table instead -- proving the two tables are actually
    // distinct, not one silently shadowing the other.
    QCOMPARE(ConnectionManager::messageKeyForError(
                     errorView(RELDEX_ERROR_KIND_OTHER, true, RELDEX_CREDENTIAL_ERROR_LOCKED), false),
             QStringLiteral("error.other"));
}

void TstConnectionManager::defaultStorePathMirrorsAdr0006PlatformRules()
{
    // An unset (or relative) variable: no derivable data directory, matching
    // `reldex_workspace::store::default_data_dir()` returning `None`.
    QVERIFY(ConnectionManager::resolveDefaultStorePath([](const char *) { return QString(); })
                    .isEmpty());
    QVERIFY(ConnectionManager::resolveDefaultStorePath(
                    [](const char *) { return QStringLiteral("relative/path"); })
                    .isEmpty());

    const QString path = ConnectionManager::resolveDefaultStorePath(
            [](const char *name) { return QStringLiteral("/tmp/reldex-test-%1").arg(name); });
    QVERIFY(path.endsWith(QStringLiteral("/com.reldex.reldex/reldex.sqlite3")));
#if defined(Q_OS_WIN)
    QVERIFY(path.contains(QStringLiteral("LOCALAPPDATA")));
#elif defined(Q_OS_MACOS)
    QVERIFY(path.contains(QStringLiteral("/Library/Application Support")));
#else
    QVERIFY(path.contains(QStringLiteral("XDG_DATA_HOME")));
#endif
}

void TstConnectionManager::aRealOnDiskWorkspaceRoundTripsThroughTheResolvedDefaultPath()
{
    // This one test opens a REAL, file-backed store (not
    // `RELDEX_WORKSPACE_IN_MEMORY`) -- proving `reldex_workspace_open()`
    // itself creates the directory/file (`Store::open_creating`, the
    // must-fix 2 fix) -- while still never touching a real OS credential
    // store. `kPlatformDataDirEnvVar` stands in for the platform's data-dir
    // env var, redirected at a `QTemporaryDir`, never the developer's own.
    m_inMemory.reset();
    QTemporaryDir tempDir;
    QVERIFY(tempDir.isValid());
    const EnvGuard dataDir(kPlatformDataDirEnvVar, tempDir.path().toUtf8());
    qunsetenv("RELDEX_WORKSPACE_PATH");
    qunsetenv("RELDEX_WORKSPACE_IN_MEMORY");

    const QString expectedPath = ConnectionManager::resolveDefaultStorePath();
    QVERIFY(!expectedPath.isEmpty());
    QVERIFY(!QFileInfo::exists(expectedPath));

    {
        Bridge bridge;
        ConnectionManager *cm = bridge.connections();
        QVERIFY(cm->open());
        QVERIFY(spinUntil([cm] { return cm->isReady(); }));
        QVERIFY(QFileInfo::exists(expectedPath));

        QSignalSpy saved(cm, &ConnectionManager::profileSaved);
        QVERIFY(cm->createProfile(baseProfileFields()));
        QVERIFY(spinUntil([&saved] { return saved.count() >= 1; }));
    }

    QVERIFY(QFileInfo::exists(expectedPath));
#if defined(Q_OS_UNIX)
    const QFileInfo info(expectedPath);
    QCOMPARE(info.permissions()
                     & (QFileDevice::ReadGroup | QFileDevice::WriteGroup | QFileDevice::ReadOther
                        | QFileDevice::WriteOther),
             QFileDevice::Permissions());
#endif

    // Reopening the same file finds the profile still there.
    Bridge bridge2;
    ConnectionManager *cm2 = bridge2.connections();
    QVERIFY(cm2->open());
    QVERIFY(spinUntil([cm2] { return cm2->isReady(); }));
    QVERIFY(spinUntil([cm2] { return cm2->profiles()->rowCount() == 1; }));
}

QTEST_GUILESS_MAIN(TstConnectionManager)

#include "tst_connectionmanager.moc"
