#pragma once

// `SettingsController` -- M4.7: the first adapter-side consumer of
// `reldex-workspace`'s settings registry (ADR-0006 P2:
// `effective = worksheet ?? profile ?? application ?? built-in`, with
// provenance). `ConnectionManager`'s own class documentation says plainly
// "it does not resolve settings" -- this is the class that does, kept
// separate for the same reason `ObjectBrowserModel` opens its own session
// rather than reusing a worksheet's: one small, focused adapter object per
// concern, not a shared one growing new responsibilities (`AGENTS.md` "Keep
// public APIs small").
//
// It owns its own workspace service thread -- a second `ReldexWorkspace*`
// handle on the same store `ConnectionManager` already opened, which
// ADR-0006 P6 says plainly is safe ("two handles on one file ... are safe:
// WAL readers never block the writer"). Reusing `ConnectionManager`'s handle
// instead was considered and rejected: that class's own documentation scopes
// it to profiles/CRUD/test-connect, and reaching into its private workspace
// handle from here would make the two classes' request/reply bookkeeping
// (`m_pending`, `m_nextRequestId`, ...) interfere with each other for no
// benefit -- a second handle costs one more SQLite connection and one more
// idle thread, which is cheap next to that coupling.
//
// The three calls below (`resolve`/`setValue`/`clearValue`) are generic over
// `SettingId` -- nothing here is server-output-specific -- so the same class
// serves `ui/README.md`'s already-documented next user (the metadata row cap,
// "A related, non-FFI gap: the row cap has no setting yet") without change.
//
// Threading/ownership mirrors `ConnectionManager` exactly: never opened
// automatically, a waker that does one thing (a coalesced queued
// `invokeMethod` back to `drain()`), and a request/reply table keyed by this
// class's own request ids (the workspace reply carries a `SettingId` only for
// `SettingResolved` -- not for `SettingSet`/`SettingCleared` -- so this class
// remembers which setting a pending set/clear was for).

#include "ReldexHandles.h"

#include <QAtomicInt>
#include <QByteArray>
#include <QHash>
#include <QObject>
#include <QPair>
#include <QString>

class SettingsController : public QObject
{
    Q_OBJECT

public:
    /// A `ReldexSettingValue`, flattened the same way the ABI struct is --
    /// only the field(s) `kind` (a `ReldexValueKind`) documents are
    /// meaningful. Plain data, not a `Q_GADGET`: nothing here crosses into
    /// QML (`ServerOutputController` is the typed, QML-facing wrapper).
    struct Value
    {
        int kind = RELDEX_VALUE_KIND_UNKNOWN;
        bool boolValue = false;
        quint32 countValue = 0;
        bool noLimit = false;
        quint32 numberValue = 0;

        [[nodiscard]] static Value ofBool(bool value) noexcept
        {
            Value result;
            result.kind = RELDEX_VALUE_KIND_BOOL;
            result.boolValue = value;
            return result;
        }
        [[nodiscard]] static Value ofByteLimitBytes(quint32 bytes) noexcept
        {
            Value result;
            result.kind = RELDEX_VALUE_KIND_BYTE_LIMIT;
            result.numberValue = bytes;
            return result;
        }
        [[nodiscard]] static Value ofByteLimitUnlimited() noexcept
        {
            Value result;
            result.kind = RELDEX_VALUE_KIND_BYTE_LIMIT;
            result.noLimit = true;
            return result;
        }
    };

    explicit SettingsController(QObject *parent = nullptr);
    ~SettingsController() override;

    [[nodiscard]] bool isReady() const noexcept { return m_ready; }

    /// Opens the workspace's service thread. Idempotent: a call while already
    /// open, or already opening, is a no-op that returns `true`. Never called
    /// automatically -- the first caller that needs a setting calls this
    /// (`ServerOutputController::open()`), same rule as
    /// `ConnectionManager::open()` and for the same reason (see that class's
    /// documentation): nothing should touch the on-disk store just because a
    /// `QObject` was constructed.
    bool open();

    /// Issues `reldex_workspace_resolve_setting`. `profileId`/`worksheetId`
    /// empty means "no such layer" (both map to a null pointer across the
    /// ABI). Returns the request id (nonzero) if the library accepted it; 0
    /// if it was refused outright (not ready, or an id this build does not
    /// know) -- `resolveFailed()` still fires for 0 so a caller can handle
    /// both failure paths in one place.
    quint64 resolve(int settingId, const QByteArray &profileId, const QByteArray &worksheetId);
    /// Issues `reldex_workspace_set_setting` at `level`
    /// (`RELDEX_SETTING_LEVEL_APPLICATION`/`_PROFILE`/`_WORKSHEET`, never
    /// `_BUILT_IN`). `scopeId` is ignored for `Application`.
    quint64 setValue(int settingId, int level, const QByteArray &scopeId, const Value &value);
    /// Issues `reldex_workspace_clear_setting`, so the setting is inherited
    /// again.
    quint64 clearValue(int settingId, int level, const QByteArray &scopeId);

    /// Persists a blank placeholder worksheet row for `worksheetId` via
    /// `reldex_workspace_save_worksheet` (empty title/text, no profile,
    /// caret/scroll/tab order 0) -- idempotent, since that call "inserts it
    /// if `id` is new, otherwise replaces its state ... in place" (its own
    /// doc comment).
    ///
    /// This exists only to satisfy a foreign key:
    /// `crates/workspace/src/store/schema.rs`'s `worksheet_setting` table
    /// declares `worksheet_id ... REFERENCES worksheet (id)`, so
    /// `setValue`/`clearValue` at `RELDEX_SETTING_LEVEL_WORKSHEET` fail for
    /// any id that was only ever generated
    /// (`reldex_workspace_new_worksheet_id`) and never saved -- found by
    /// `tst_serveroutput.cpp`'s settings-round-trip test, which could
    /// resolve (read) a worksheet-scoped setting but never saw a write to it
    /// take effect. `resolve()` is unaffected: a resolve with no matching
    /// row simply falls through to the next level, so callers that only
    /// ever read a worksheet-scoped setting never need this. A caller that
    /// writes one -- `ServerOutputController::open()`, and any future
    /// worksheet-scoped setting consumer -- must call this once and wait for
    /// its reply before the first `setValue`/`clearValue` at Worksheet level
    /// for that id.
    quint64 ensureWorksheet(const QByteArray &worksheetId);

Q_SIGNALS:
    void readyChanged();
    void openFinished(bool ok);

    void resolved(quint64 request, int settingId, SettingsController::Value value, int level);
    void resolveFailed(quint64 request, int settingId, const QString &message);
    void valueSet(quint64 request, int settingId);
    void setFailed(quint64 request, int settingId, const QString &message);
    void valueCleared(quint64 request, int settingId);
    void clearFailed(quint64 request, int settingId, const QString &message);
    void worksheetEnsured(quint64 request);
    void worksheetEnsureFailed(quint64 request, const QString &message);

private:
    enum class PendingKind { Resolve, Set, Clear, SaveWorksheet };

    friend void reldexSettingsControllerWakeImpl(void *userData) noexcept;

    void postDrain();
    void drain();
    void handleReply(const ReldexWorkspaceReply &reply);
    [[nodiscard]] quint64 nextRequest();

    reldex::WorkspaceHandle m_workspace;
    bool m_ready = false;
    quint64 m_openRequest = 0;
    quint64 m_nextRequestId = 1;
    /// Request id -> (what it was, which setting it named). The reply itself
    /// only names the setting back for `SettingResolved`
    /// (`ReldexWorkspaceReply::setting_id`'s own doc comment) -- this is how
    /// `SettingSet`/`SettingCleared` still get to report which one.
    QHash<quint64, QPair<PendingKind, int>> m_pending;

    QAtomicInt m_drainPosted { 0 };
    bool m_draining = false;
};

Q_DECLARE_METATYPE(SettingsController::Value)
