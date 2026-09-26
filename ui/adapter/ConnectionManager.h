#pragma once

// `ConnectionManager` -- M3.2: the thin adapter for the connection-manager UI
// (list, create/edit/delete, environment, test-connect; `SPEC.md` §17,
// ADR-0006, ADR-0007).
//
// It owns the workspace's service thread (`ReldexWorkspace*`,
// `reldex_workspace_open`/`_close`/`_set_waker`/`_next_reply`) the same way
// `Bridge` owns the hub: a waker callback that does exactly one thing (post a
// coalesced drain), and a `drain()` that takes replies until the queue is
// empty or a budget is spent. No business rule lives in QML (`AGENTS.md`):
// validation, the ADR-0007 write-order/clearing choreography, and every
// FFI-error-to-message-key mapping all live here.
//
// Scope, deliberately: this class is a `ProfileModel` (list) plus
// create/update/delete/test-connect. It does not resolve settings, does not
// touch history or worksheets, and does not implement the real connect flow
// (M3.3 owns "Connecting..."/cancel/bounded timeout) -- test-connect below is
// a throwaway probe with its own transient session, never the worksheet's.
//
// Opening: `open()` is explicit, never automatic. The workspace's service
// thread is NOT started from this class's constructor -- `Bridge` still
// constructs a `ConnectionManager` eagerly (cheap: it allocates a model and
// nothing else), but nothing calls `open()` until either the app's own
// startup path does (`Main.qml`'s `Bridge { Component.onCompleted:
// connections.open() }`) or a test does. This is a fix for a real bug (M3.2
// fix round, 2026-09-26): opening eagerly from the constructor meant *every*
// test that merely constructs a `Bridge` -- `tst_bridge`, `tst_resultmodel`,
// `tst_teardown` (12,000 constructions in its K5 loop), and `tst_coreinfo`/
// its DPI variants (which load the real `Main.qml`) -- silently opened the
// developer's real on-disk default workspace store, because none of them had
// any reason to guard against a connection-manager side effect they never
// asked for. `open()` is idempotent (a second call while already open or
// opening is a no-op) and `Q_INVOKABLE` so both QML and a test can call it
// directly.
//
// FFI gaps this class works around, documented in the M3.2 hand-off (see
// ui/README.md "Connection manager (M3.2)"):
//  * `reldex_hub_open_session` accepts only `RELDEX_DRIVER_KIND_MOCK` in this
//    build, and the mock cannot be configured to fail its open or a ping --
//    so `testConnect()` proves the pipeline (build params -> open -> ping ->
//    close) rather than a real socket to the target database. A profile's own
//    typed failures (an invalid profile, a missing password) are still real,
//    caught entirely by `reldex_workspace_build_connect_params` before any
//    session is opened.
//  * `ProfileError` (unlike `CredentialError`) crosses the ABI folded into
//    `RELDEX_ERROR_KIND_CONFIGURATION` with no numeric sub-code -- including
//    the credential-pattern refusal. Its `Display` text is safe by
//    construction (`ProfileError::CredentialInEndpoint` "names the field and
//    the pattern class and never the text", `crates/workspace/src/
//    profile.rs`), so this class shows that message verbatim under one
//    message key (`error.configuration`) rather than inventing a fragile
//    English-text parse to recover a finer one. Deferred to M2.15.
//
// Fixed since the first version of this class (M3.2 fix round, 2026-09-26):
// `reldex_workspace_open` now creates a non-in-memory store's directory and
// file itself, with ADR-0006 P5's permissions, on its own service thread
// (`Store::open_creating`, `crates/ffi/src/workspace.rs`) -- this class no
// longer reimplements that on the C++ side (there is no
// `ensureStoreDirectoryReady()` any more), and never did it on a UI thread.

#include "ProfileModel.h"
#include "ReldexHandles.h"

#include <QByteArray>
#include <QHash>
#include <QObject>
#include <QPointer>
#include <QString>
#include <QVariantMap>
#include <QtQml/qqmlregistration.h>

#include <functional>

class Bridge;

class ConnectionManager : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ConnectionManager is created by Bridge and reached as bridge.connections")

    /// True once `open()` has actually opened a workspace handle (whether or
    /// not it has finished; `ready` is that). False before `open()` is ever
    /// called, and false again if it failed (an OS thread-creation failure or
    /// an invalid argument) -- `open()` may be retried in that case, which is
    /// why this is `NOTIFY`, not `CONSTANT`: unlike `Bridge::valid` (decided
    /// once, synchronously, in its constructor), this can change after
    /// construction now that opening is no longer automatic.
    Q_PROPERTY(bool valid READ isValid NOTIFY validChanged)
    /// True once `RELDEX_WORKSPACE_REPLY_KIND_OPENED` has been drained
    /// without an error. Every command below is refused (returns `false`,
    /// `hasError`/`errorMessageKey` set to `error.workspace.notReady`) before
    /// this is true.
    Q_PROPERTY(bool ready READ isReady NOTIFY readyChanged)
    Q_PROPERTY(ProfileModel *profiles READ profiles CONSTANT)
    /// A `ReldexCredentialStoreKind`. `RELDEX_CREDENTIAL_STORE_KIND_UNKNOWN`
    /// until `ready`.
    Q_PROPERTY(int credentialStoreKind READ credentialStoreKind NOTIFY readyChanged)
    /// M2.10 hand-off: a connection dialog offers "save password" only when
    /// this is true, and otherwise saves the profile as prompt-each-time.
    Q_PROPERTY(bool canStoreCredential READ canStoreCredential NOTIFY readyChanged)

    /// The most recent command's failure, if any -- cleared at the start of
    /// the next command. `errorMessage` is always safe to show verbatim: it
    /// is either `ReldexErrorView::message` (a `ProfileError`/`ConnectError`
    /// Display, value-free by construction) or a `CredentialError`'s own
    /// value-free Display (ADR-0007 S2).
    Q_PROPERTY(bool hasError READ hasError NOTIFY errorChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorChanged)
    /// A short, stable key such as `error.configuration` or
    /// `error.credential.locked` -- see the class documentation's "each FFI
    /// error kind maps to a message key" table, restated in
    /// `messageKeyForError()`'s own comment.
    Q_PROPERTY(QString errorMessageKey READ errorMessageKey NOTIFY errorChanged)
    /// The raw `ReldexErrorKind`, for a caller that wants it (diagnostics,
    /// tests); `errorMessageKey` is the one meant for display.
    Q_PROPERTY(int errorKind READ errorKind NOTIFY errorChanged)

    /// True from `testConnect()` until its success/failure signal fires. The
    /// dialog uses this to disable a second concurrent probe -- test-connect
    /// is single-flight by design (its state is a handful of plain members,
    /// not a request table, exactly because there is only ever one).
    Q_PROPERTY(bool testConnectBusy READ testConnectBusy NOTIFY testConnectStateChanged)

public:
    explicit ConnectionManager(Bridge *bridge, QObject *parent = nullptr);
    ~ConnectionManager() override;

    /// Opens the workspace's service thread. Idempotent: a call while
    /// already open, or already opening, is a no-op that returns `true`.
    /// Never called automatically (see the class documentation) -- the app's
    /// startup path or a test calls this explicitly. Returns whether the
    /// open request was issued, not whether it has finished; the outcome
    /// arrives through `readyChanged()`/`validChanged()`/`openFinished()`.
    Q_INVOKABLE bool open();

    [[nodiscard]] bool isValid() const noexcept { return m_workspace != nullptr; }
    [[nodiscard]] bool isReady() const noexcept { return m_ready; }
    [[nodiscard]] ProfileModel *profiles() const noexcept { return m_profiles; }
    [[nodiscard]] int credentialStoreKind() const noexcept { return m_credentialStoreKind; }
    [[nodiscard]] bool canStoreCredential() const noexcept
    {
        return reldex_credential_store_kind_can_store(m_credentialStoreKind);
    }

    [[nodiscard]] bool hasError() const noexcept { return m_hasError; }
    [[nodiscard]] QString errorMessage() const { return m_errorMessage; }
    [[nodiscard]] QString errorMessageKey() const { return m_errorMessageKey; }
    [[nodiscard]] int errorKind() const noexcept { return m_errorKind; }

    [[nodiscard]] bool testConnectBusy() const noexcept { return m_testConnectBusy; }
    /// Whether the test-connect in progress (or the last one) used a password
    /// `resolve_password` resolved `FromStore`, rather than one typed in.
    /// Public (C++-only, not exposed to QML -- `testConnectFailed`'s own
    /// fourth argument is what a dialog reads) so a test can check the wiring
    /// end to end without befriending the class.
    [[nodiscard]] bool testConnectUsedStoredPassword() const noexcept
    {
        return m_testConnectUsedStoredPassword;
    }

    // --- commands. Every one validates/maps in this class, never in QML. --

    /// `fields` keys mirror `ProfileModel::get()`'s output (see that class):
    /// name, environment, environmentLabel, treatAsProduction, endpointKind,
    /// host, port, serviceTargetKind, serviceNameOrSid, connectString,
    /// username, authKind, passwordStorage, password, sessionRole,
    /// transport, caDirectory, allowUnenforcedCertificatePin.
    ///
    /// `passwordStorage` is the user's *requested* choice;
    /// `RELDEX_PASSWORD_STORAGE_KIND_CREDENTIAL_STORE` with a non-empty
    /// `password` saves it (ADR-0007 write order: `put` first, then the
    /// profile carries the flag only if `put` succeeded). Never blocks; the
    /// outcome arrives as `profileSaved`/`profileSaveFailed`.
    Q_INVOKABLE bool createProfile(const QVariantMap &fields);
    /// As `createProfile`, replacing `idHex`'s details. Also runs the
    /// ADR-0007 clearing rule when `passwordStorage` moves away from
    /// `CredentialStore` (or `authKind` moves away from `Password`): the
    /// stored entry is deleted first, and the flag flips to prompt-each-time
    /// whether or not that delete succeeded (a failure is reported through
    /// `credentialWarning`, never silently dropped).
    Q_INVOKABLE bool updateProfile(const QString &idHex, const QVariantMap &fields);
    /// Deletes the stored credential (best-effort; a failure is reported
    /// through `credentialWarning`, and the profile is deleted either way --
    /// ADR-0007 Consequences), then the profile itself.
    Q_INVOKABLE bool deleteProfile(const QString &idHex);
    /// Builds connect params for `idHex` (validating the stored profile and,
    /// with a non-empty `password`, using exactly that text -- never the
    /// credential store, never `resolve_password`: this dialog's password
    /// field is "used only for test-connect or 'save to credential store'",
    /// per the M3.2 task brief), then opens a throwaway session on the
    /// existing hub, pings it, and closes it. See the class documentation for
    /// why this build's mock driver is what actually gets opened.
    Q_INVOKABLE bool testConnect(const QString &idHex, const QString &password);

    /// The default `getenv` for `resolveDefaultStorePath()` below:
    /// `QProcessEnvironment::systemEnvironment()`. Public only because a
    /// default-argument expression is access-checked at the call site in
    /// C++, and this default must be usable from outside the class; it does
    /// nothing a caller could not already do with `qEnvironmentVariable()`.
    [[nodiscard]] static QString defaultGetEnv(const char *name);

    /// ADR-0006 P5's *path* rule (mirrors
    /// `reldex_workspace::store::default_data_dir()`) -- only the path, not
    /// the directory/file creation any more (that moved to
    /// `reldex_workspace_open`'s own service thread, `Store::open_creating`;
    /// see the class documentation). `open()` still needs to know which path
    /// to pass across the ABI when no explicit override is given. Pure and
    /// independently testable: `getenv` defaults to `qEnvironmentVariable`,
    /// and a test overrides it to point at a `QTemporaryDir` without touching
    /// the process environment. Returns empty when the platform has no
    /// derivable data directory (mirrors `default_data_dir()` returning
    /// `None`).
    [[nodiscard]] static QString
    resolveDefaultStorePath(const std::function<QString(const char *)> &getenv = defaultGetEnv);

    /// "Each FFI error kind maps to a message key" -- public and pure so
    /// `tst_connectionmanager` can check the whole table directly. `credentialContext`:
    /// true for a `reldex_workspace_credential_*` reply (native code is a
    /// `ReldexCredentialError`), false for everything else.
    [[nodiscard]] static QString messageKeyForError(const ReldexErrorView &view,
                                                     bool credentialContext);

    /// The decision `handleHubEvent()` makes for `testConnectFailed`'s
    /// `authFailureWithStoredPassword`: true only for an authentication
    /// failure *and* a test-connect that used a password `resolve_password`
    /// resolved `FromStore`. Public, pure and taking already-decoded inputs
    /// (rather than a `ReldexEvent`/`ReldexError`) for the same reason
    /// `messageKeyForError()` is: this build's mock driver cannot itself
    /// produce a real authentication failure to drive the real path with
    /// (`crates/ffi` exposes no way to fabricate a `ReldexError*` either, so
    /// a "fake hub event" through the real private method is not buildable
    /// without a core change this task's scope excludes) -- the decision
    /// itself is still fully exercised this way, independent of whether the
    /// failure it decides about can be produced in this build.
    [[nodiscard]] static bool isAuthFailureAgainstStoredPassword(int errorKind,
                                                                  bool usedStoredPassword) noexcept
    {
        return errorKind == RELDEX_ERROR_KIND_AUTHENTICATION && usedStoredPassword;
    }

Q_SIGNALS:
    void validChanged();
    void readyChanged();
    void errorChanged();
    void testConnectStateChanged();

    void profileSaved(const QString &idHex, bool created);
    void profileSaveFailed(const QString &messageKey, const QString &message);
    void profileDeleted(const QString &idHex);
    void profileDeleteFailed(const QString &messageKey, const QString &message);
    /// A non-fatal warning alongside an otherwise-successful save/delete --
    /// ADR-0007's "leftover entry reported" and "password not saved" cases.
    void credentialWarning(const QString &messageKey, const QString &message);

    void testConnectSucceeded(const QString &idHex, const QString &summary);
    /// `authFailureWithStoredPassword`: ADR-0007's rule -- a stored password
    /// is never retried automatically; the UI offers to update it instead.
    /// True only when `testConnect()` actually used a password
    /// `resolve_password` resolved `FromStore` (never when the user typed a
    /// password explicitly, and never just because the profile's own
    /// `passwordStorage` says `CredentialStore` -- that flag can be stale, so
    /// `resolve_password` is asked rather than trusted locally) *and* the
    /// resulting session open/ping failed with `RELDEX_ERROR_KIND_
    /// AUTHENTICATION`. This build's mock driver cannot itself fail an open
    /// or a ping (see the class documentation), so end-to-end coverage of the
    /// "database refuses it" half is not possible here; see
    /// `isAuthFailureAgainstStoredPassword()`'s own doc comment for how the
    /// *decision* is tested instead.
    void testConnectFailed(const QString &idHex, const QString &messageKey, const QString &message,
                            bool authFailureWithStoredPassword);

    /// Test-only: fires once the OPENED reply for a workspace has been
    /// drained, whether or not it succeeded. `tst_connectionmanager` spins on
    /// this rather than on `ready` so a failed open (which never sets
    /// `ready`) is still observable without a bare timeout.
    void openFinished(bool ok);

private:
    struct ProfileFields
    {
        QString name;
        int environment = RELDEX_ENVIRONMENT_KIND_DEVELOPMENT;
        QString environmentLabel;
        bool treatAsProduction = false;
        int endpointKind = RELDEX_ENDPOINT_KIND_HOST_PORT;
        QString host;
        int port = 0;
        int serviceTargetKind = RELDEX_SERVICE_TARGET_KIND_SERVICE_NAME;
        QString serviceNameOrSid;
        QString connectString;
        int authKind = RELDEX_AUTH_KIND_PASSWORD;
        QString username;
        /// The value actually written to `ReldexProfileDetails` at whatever
        /// step is about to run -- callers rewrite this between steps as the
        /// ADR-0007 choreography advances, rather than carrying a separate
        /// "requested" field.
        int passwordStorage = RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
        int role = RELDEX_SESSION_ROLE_KIND_NORMAL;
        int transport = RELDEX_TRANSPORT_KIND_PLAIN;
        QString caDirectory;
        bool allowUnenforcedCertificatePin = false;
    };

    /// UTF-8 backing storage plus the `ReldexProfileDetails` view into it.
    /// Kept together so the struct is never separated from the bytes it
    /// borrows (ADR-0003's "into Reldex" rule: valid for the call's duration
    /// only).
    struct OwnedProfileDetails
    {
        QByteArray name, environmentLabel, host, serviceNameOrSid, connectString, username,
                caDirectory;
        ReldexProfileDetails details {};
    };

    enum class OpKind { CreateProfile, UpdateProfile, DeleteProfile };
    enum class Step {
        CreateAwaitingSave,
        CreateAwaitingCredentialPut,
        CreateAwaitingFlagUpdate,
        UpdateAwaitingCredentialPut,
        UpdateAwaitingCredentialDelete,
        UpdateAwaitingSave,
        DeleteAwaitingCredentialDelete,
        DeleteAwaitingDelete,
    };

    struct PendingOp
    {
        OpKind kind;
        Step step;
        QByteArray profileId; // empty until Create's first reply names one
        ProfileFields fields;
        QString passwordToPut;
        /// What `fields.passwordStorage` must end up as once the credential
        /// step (if any) resolves -- set once, read when that step's reply
        /// arrives.
        int finalPasswordStorage = RELDEX_PASSWORD_STORAGE_KIND_PROMPT_EACH_TIME;
    };

    [[nodiscard]] static ProfileFields fieldsFromMap(const QVariantMap &map);
    [[nodiscard]] static OwnedProfileDetails buildDetails(const ProfileFields &fields);
    [[nodiscard]] static ProfileModel::Row rowFromView(const ReldexProfileView &view);
    [[nodiscard]] static QString endpointSummaryFor(const ReldexProfileView &view);
    [[nodiscard]] static QByteArray idBytes(const QString &idHex);
    [[nodiscard]] static QString idHexOf(const QByteArray &id16);

    void adoptError(const ReldexError *error, bool credentialContext);
    void clearError();

    [[nodiscard]] bool ensureReady();
    [[nodiscard]] quint64 nextRequest();

    void postDrain();
    void drain();
    void handleReply(const ReldexWorkspaceReply &reply);
    void continueCreateProfile(quint64 request, PendingOp &op, const ReldexWorkspaceReply &reply);
    void continueUpdateProfile(quint64 request, PendingOp &op, const ReldexWorkspaceReply &reply);
    void continueDeleteProfile(quint64 request, PendingOp &op, const ReldexWorkspaceReply &reply);
    void finishSave(const PendingOp &op, const QByteArray &id, bool created);

    /// Issues `reldex_workspace_build_connect_params` for the profile
    /// `testConnect()` is probing, with `password` (which may be null --
    /// "no password available", not an error) as-is; stashes the request id
    /// so `handleConnectParamsBuilt()` recognizes its reply. On a rejected
    /// request (the FFI call itself failing, not a later async error), fails
    /// test-connect immediately and returns `false`.
    bool issueBuildConnectParams(const QString &idHex, const ReldexSecret *password);
    void handleConnectParamsBuilt(const ReldexWorkspaceReply &reply);
    /// `testConnect()`'s continuation when no password was typed: reads
    /// where `resolve_password` says one should come from
    /// (`m_testConnectUsedStoredPassword`, ADR-0007 S3) and, on `FromStore`,
    /// forwards `reply.secret` straight into `issueBuildConnectParams()`
    /// (used synchronously within this same drain iteration; never adopted
    /// or retained -- `drain()`'s own `SecretHandle` releases it right after
    /// `handleReply()` returns, same as every other reply-owned object here).
    void handlePasswordResolved(const ReldexWorkspaceReply &reply);
    void handleHubEvent(const ReldexEvent &raw, reldex::BatchHandle batch, reldex::ErrorHandle error);
    void finishTestConnect();

    friend void reldexConnectionManagerWakeImpl(void *userData) noexcept;
    friend class Bridge;

    /// `QPointer`, not a plain `Bridge *` (M3.2 round-2 fix, 2026-09-26): a
    /// raw pointer stays non-null even after the pointee is destroyed, so a
    /// `m_bridge != nullptr` guard against a dangling `Bridge` was always a
    /// false positive -- it only ever caught a genuinely-null bridge, which
    /// never happens (the constructor is always given a valid one). A
    /// `QPointer` self-nulls once `Bridge` starts destroying itself (before
    /// `QObjectPrivate::deleteChildren()` gets to this object), which is what
    /// makes every guard below a real check rather than a no-op. This is
    /// deliberately redundant with `Bridge::~Bridge()` now explicitly
    /// deleting this object before its own data members are torn down (see
    /// that destructor's comment) -- the invariant "never dereference a gone
    /// `Bridge`" must not depend on which of the two ever fires first.
    QPointer<Bridge> m_bridge;
    reldex::WorkspaceHandle m_workspace;
    ProfileModel *m_profiles = nullptr;

    bool m_ready = false;
    int m_credentialStoreKind = RELDEX_CREDENTIAL_STORE_KIND_UNKNOWN;

    bool m_hasError = false;
    QString m_errorMessage;
    QString m_errorMessageKey;
    int m_errorKind = RELDEX_ERROR_KIND_UNKNOWN;

    quint64 m_nextRequestId = 1;
    QHash<quint64, PendingOp> m_pending;
    /// The one open request (`reldex_workspace_open`'s) reply is not a
    /// `PendingOp`: nothing chains after it, and it is the one reply that can
    /// arrive before `m_ready`.
    quint64 m_openRequest = 0;

    // --- test-connect: single-flight, plain members (see the property's
    // own doc comment for why this is not a `PendingOp`). ---------------
    bool m_testConnectBusy = false;
    QByteArray m_testConnectProfileId;
    /// Set when no password was typed, while `resolve_password`'s reply is
    /// awaited; 0 once `issueBuildConnectParams()` has run (whether reached
    /// from here or directly from `testConnect()` for a typed password).
    quint64 m_testConnectResolveRequest = 0;
    /// True only when this test-connect's password came from
    /// `resolve_password` resolving `FromStore` -- the one case
    /// `handleHubEvent()` may set `authFailureWithStoredPassword` for. False
    /// for a typed password, `PromptRequired`, or `NotNeeded` alike.
    bool m_testConnectUsedStoredPassword = false;
    quint64 m_testConnectBuildRequest = 0;
    quint64 m_testConnectSessionId = 0;
    quint64 m_testConnectPingRequest = 0;
    quint64 m_testConnectCloseRequest = 0;
    QString m_testConnectSummary;

    QAtomicInt m_drainPosted { 0 };
    bool m_draining = false;
};
