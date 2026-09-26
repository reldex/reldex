#pragma once

// `ServerOutputController` -- M4.7: the DBMS_OUTPUT pane's adapter object,
// reached from QML as `bridge.serverOutput`.
//
// It ties together the three pieces `phase-1.md` row M4.7 asks for:
//
//  * `SettingsController` (ADR-0006 P2), for `server_output.enabled`/
//    `server_output.buffer` at *this* worksheet's scope --
//    `effective = worksheet ?? profile ?? application ?? built-in`, with
//    provenance (`enabledLevel`/`bufferLevel`, a `ReldexSettingLevel`).
//  * its own dedicated `SessionController` (a second, independent mock
//    session on the same hub `bridge.session` uses -- the same pattern
//    `ObjectBrowserModel` already uses for its own metadata session,
//    "distinct from any worksheet's session", `ui/README.md` "Object
//    browser (M6.1)"), which is what actually calls
//    `reldex_session_set_server_output` and receives `SERVER_OUTPUT` events.
//  * `ServerOutputModel`, the virtualized line list the pane's `ListView`
//    binds to.
//
// **Hand-off (read before wiring a second instance into the worksheet
// area).** As of M3.4, every worksheet tab still shares the one
// `SessionController`/session `Bridge` owns (`ui/README.md` "Production
// indicator (M3.4)"); per-tab sessions are M4.9's "N sessions, per-tab
// state" work. `bridge.serverOutput` mirrors that exact maturity level: one
// instance, representing whichever worksheet is active, with its own
// worksheet id (`reldex_workspace_new_worksheet_id()`, generated once when
// this `Bridge` is constructed) so its *settings* are already genuinely
// worksheet-scoped even though its *session* is a stand-in. The class itself
// does not assume there is only one -- `ui/tests/tst_serveroutput.cpp`
// constructs two independent instances, sharing one `SettingsController`
// but each with its own worksheet id, and proves their settings (and their
// displayed lines, since each owns its own session) are isolated. Wiring one
// instance per worksheet tab is additive from here: construct another
// `ServerOutputController` with another worksheet id and the same
// `SettingsController*`.
//
// **Hand-off (the sample-statement button).** Real `DBMS_OUTPUT` is a side
// effect of the user's own PL/SQL, run through a worksheet's *execution*
// session -- there is no such session, and no real editor to type into yet
// (M4.1). `runSampleStatement()` is this build's stand-in, the same role
// `Bridge::run()`/`SessionController::runOnOpen` already play for the result
// grid: it runs `RELDEX_MOCK_STATEMENT_SERVER_OUTPUT` on this pane's own
// session. Once a worksheet owns a real execution session, that session's
// `serverOutputReceived`/`serverOutputConfigured` signals are exactly what
// this class already knows how to consume -- only the "whose session is
// this" wiring changes, not this class's shape.
//
// No business rule lives in QML (`AGENTS.md`): the pane's static "your own
// DBMS_OUTPUT calls override this" copy is presentation text, but every
// bound/level/truncation *value* it shows comes from here.

#include "ReldexHandles.h"
#include "ServerOutputModel.h"
#include "SettingsController.h"

#include <QByteArray>
#include <QObject>
#include <QPointer>
#include <QSet>
#include <QString>
#include <QtQml/qqmlregistration.h>

class Bridge;
class SessionController;

class ServerOutputController : public QObject
{
    Q_OBJECT
    QML_ELEMENT
    QML_UNCREATABLE("ServerOutputController is created by Bridge and reached as bridge.serverOutput")

    Q_PROPERTY(ServerOutputModel *model READ model CONSTANT)
    /// True once this pane's session is open, its two settings
    /// (`enabled`/buffer) have each been resolved at least once, and its
    /// worksheet row has been persisted (`SettingsController::ensureWorksheet`)
    /// -- so `setEnabled`/`setBufferBytes`/etc. are safe to call from here on
    /// without racing the foreign key `worksheet_setting` has on `worksheet`.
    Q_PROPERTY(bool ready READ ready NOTIFY readyChanged)

    Q_PROPERTY(bool enabled READ enabled NOTIFY enabledChanged)
    /// A `ReldexSettingLevel` (`SettingLevel` below): where `enabled`'s value
    /// came from.
    Q_PROPERTY(int enabledLevel READ enabledLevel NOTIFY enabledChanged)
    Q_PROPERTY(bool unlimited READ unlimited NOTIFY bufferChanged)
    /// Meaningless while `unlimited` is true (ADR-0006 P2: "no limit" is a
    /// value, never conflated with a number).
    Q_PROPERTY(quint32 bufferBytes READ bufferBytes NOTIFY bufferChanged)
    Q_PROPERTY(int bufferLevel READ bufferLevel NOTIFY bufferChanged)
    /// The registry's own bounds for `server_output.buffer`
    /// (ADR-0006 P2), so the pane's size control never has to duplicate
    /// them.
    Q_PROPERTY(quint32 minBufferBytes READ minBufferBytes CONSTANT)
    Q_PROPERTY(quint32 maxBufferBytes READ maxBufferBytes CONSTANT)

    /// Cumulative since this pane last cleared: `server_output_dropped`
    /// summed across every `SERVER_OUTPUT`/`TERMINAL` this session has seen
    /// (each event's own count is incremental -- ADR-0003 A34/A35).
    Q_PROPERTY(quint32 droppedLines READ droppedLines NOTIFY truncationChanged)
    /// Cumulative `server_output_invalid_utf8_lines` (M2.12): a count, never
    /// shown as raw bytes in the line text itself.
    Q_PROPERTY(quint32 invalidUtf8Lines READ invalidUtf8Lines NOTIFY truncationChanged)
    /// A `SERVER_OUTPUT` read has failed at least once since the last clear:
    /// the output is known incomplete.
    Q_PROPERTY(bool readFailed READ readFailed NOTIFY truncationChanged)
    Q_PROPERTY(QString readFailureMessage READ readFailureMessage NOTIFY truncationChanged)

public:
    /// A `ReldexSettingLevel`, mirrored so QML can compare against a named
    /// value (`ServerOutputController.LevelWorksheet`) instead of a bare
    /// int -- same pattern as `SessionController::State`.
    enum SettingLevel {
        LevelUnknown = RELDEX_SETTING_LEVEL_UNKNOWN,
        LevelBuiltIn = RELDEX_SETTING_LEVEL_BUILT_IN,
        LevelApplication = RELDEX_SETTING_LEVEL_APPLICATION,
        LevelProfile = RELDEX_SETTING_LEVEL_PROFILE,
        LevelWorksheet = RELDEX_SETTING_LEVEL_WORKSHEET,
    };
    Q_ENUM(SettingLevel)

    /// `settings` is borrowed (owned by `Bridge`, shared by every worksheet's
    /// pane); `worksheetId` is this pane's own 16-byte `WorksheetId`.
    explicit ServerOutputController(Bridge *bridge, SettingsController *settings,
                                    QByteArray worksheetId, QObject *parent = nullptr);
    ~ServerOutputController() override;

    [[nodiscard]] ServerOutputModel *model() const noexcept { return m_model; }
    [[nodiscard]] bool ready() const noexcept { return m_ready; }

    [[nodiscard]] bool enabled() const noexcept { return m_enabled; }
    [[nodiscard]] int enabledLevel() const noexcept { return m_enabledLevel; }
    [[nodiscard]] bool unlimited() const noexcept { return m_unlimited; }
    [[nodiscard]] quint32 bufferBytes() const noexcept { return m_bufferBytes; }
    [[nodiscard]] int bufferLevel() const noexcept { return m_bufferLevel; }
    [[nodiscard]] static constexpr quint32 minBufferBytes() noexcept { return kMinBufferBytes; }
    [[nodiscard]] static constexpr quint32 maxBufferBytes() noexcept { return kMaxBufferBytes; }

    [[nodiscard]] quint32 droppedLines() const noexcept { return m_droppedLines; }
    [[nodiscard]] quint32 invalidUtf8Lines() const noexcept { return m_invalidUtf8Lines; }
    [[nodiscard]] bool readFailed() const noexcept { return m_readFailed; }
    [[nodiscard]] QString readFailureMessage() const { return m_readFailureMessage; }

    /// This pane's worksheet id, 16 raw bytes. C++-only (tests); QML never
    /// needs it directly.
    [[nodiscard]] const QByteArray &worksheetId() const noexcept { return m_worksheetId; }

    /// Opens this pane's session and requests its current settings. Lazy --
    /// call once, when the pane is first shown (mirrors the object browser's
    /// "open on first expand"). Idempotent.
    Q_INVOKABLE bool open();

    /// Writes a worksheet-level override and re-arms the session once the
    /// write is confirmed. Returns whether the request was accepted, not
    /// whether it has completed -- `enabledChanged()` fires once the
    /// round-trip resolve that follows the write comes back.
    Q_INVOKABLE bool setEnabled(bool enabled);
    /// As `setEnabled`, for `server_output.buffer` in bytes; clamped to
    /// [`minBufferBytes`, `maxBufferBytes`] before it is written.
    Q_INVOKABLE bool setBufferBytes(quint32 bytes);
    /// As `setBufferBytes`, writing "no limit" instead of a byte count.
    Q_INVOKABLE bool setUnlimited();
    /// Removes this worksheet's override, so the setting is inherited from
    /// the profile/application/built-in level again.
    Q_INVOKABLE bool clearEnabledOverride();
    Q_INVOKABLE bool clearBufferOverride();

    /// The pane's Clear button: empties the displayed lines and the
    /// truncation/invalid-UTF-8/read-failure counters. Local UI state only
    /// -- never touches the settings registry or the session's own
    /// server-side buffer.
    Q_INVOKABLE void clear();

    /// Runs the mock's canned `DBMS_OUTPUT`-producing statement on this
    /// pane's own session. See the class documentation's hand-off note.
    Q_INVOKABLE bool runSampleStatement();

Q_SIGNALS:
    void readyChanged();
    void enabledChanged();
    void bufferChanged();
    void truncationChanged();
    /// A settings write/read/clear failed. Diagnostic only -- this build's
    /// in-memory/on-disk store has no realistic way to fail these calls, so
    /// there is no dedicated UI state for it (unlike `readFailed`, which the
    /// mock driver can and does exercise).
    void settingsError(const QString &message);

private:
    static constexpr quint32 kMinBufferBytes = 2000;
    static constexpr quint32 kMaxBufferBytes = 1024u * 1024u * 1024u;

    void requestResolveAll();
    void ensureSettingsResolveRequested();
    void maybeArmAndUpdateReady();
    void armSession();

    void handleResolved(quint64 request, int settingId, SettingsController::Value value, int level);
    void handleResolveFailed(quint64 request, int settingId, const QString &message);
    void handleSet(quint64 request, int settingId);
    void handleSetFailed(quint64 request, int settingId, const QString &message);
    void handleCleared(quint64 request, int settingId);
    void handleClearFailed(quint64 request, int settingId, const QString &message);
    void handleWorksheetEnsured(quint64 request);
    void handleWorksheetEnsureFailed(quint64 request, const QString &message);

    void handleSessionOpened();
    void handleSessionServerOutput(const QStringList &lines, quint32 dropped,
                                   quint32 invalidUtf8Lines, bool readFailed,
                                   const QString &errorMessage);

    // `tst_serveroutput.cpp`'s ordering test needs to connect directly to
    // this pane's own `SessionController::executed()` to prove
    // `SERVER_OUTPUT` is reflected in the model *before* that signal fires
    // -- same pattern as `friend class TstConnectionManager` (ConnectionManager.h)
    // / `friend class tst_ObjectBrowserModel` (ObjectBrowserModel.h).
    friend class TstServerOutputController;

    QByteArray m_worksheetId;
    QPointer<SettingsController> m_settings;
    SessionController *m_session = nullptr;
    ServerOutputModel *m_model = nullptr;

    bool m_openCalled = false;
    bool m_resolveRequested = false;
    bool m_enabledSeen = false;
    bool m_bufferSeen = false;
    bool m_worksheetEnsured = false;
    bool m_sessionReady = false;
    bool m_ready = false;

    bool m_enabled = false;
    int m_enabledLevel = RELDEX_SETTING_LEVEL_BUILT_IN;
    bool m_unlimited = false;
    quint32 m_bufferBytes = 1'000'000;
    int m_bufferLevel = RELDEX_SETTING_LEVEL_BUILT_IN;

    quint32 m_droppedLines = 0;
    quint32 m_invalidUtf8Lines = 0;
    bool m_readFailed = false;
    QString m_readFailureMessage;

    /// Request ids *this* instance issued against `m_settings`, which may be
    /// shared by other `ServerOutputController`s (a shared store, several
    /// worksheets) -- every reply this class receives on a shared
    /// `SettingsController` must be filtered through this before being
    /// believed.
    QSet<quint64> m_ownRequests;
};
