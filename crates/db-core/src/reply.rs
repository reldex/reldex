//! Where one request's answer goes.
//!
//! A request submitted through [`crate::Completion`] and the same request
//! submitted through the event path are the same [`crate::worker::Command`]
//! carrying a different [`ReplyTo`] — which is what keeps the two paths from
//! drifting apart (`docs/exec-plans/active/phase-1.md` §B2, §B5).
//!
//! # Exactly one reply, structurally
//!
//! [`ReplyTo::answer`] consumes the reply channel, so a second answer does not
//! type-check. The other half — *never zero* — is [`Drop`]: a reply channel
//! that is dropped without being answered emits the session's terminal failure
//! on the way out. That covers every path that used to need a bookkeeping set
//! of "requests we still owe": the worker exiting with commands still queued,
//! a submit whose command could not be delivered, and a panic that unwinds the
//! worker itself.
//!
//! The synthesis is **event-path only**. On the completion path a dropped
//! `Sender` already makes [`crate::Completion::wait`] report that the worker
//! vanished, and that behaviour is documented and tested; this module does not
//! change it.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use reldex_db_driver_api::{CancelKind, ConnectionId, DbResult, ServerOutputSetting, Warning};

use crate::events::{CompletedOperation, RequestId, SessionEvent};
use crate::ids::{LobHandle, ResultId, SessionId};
use crate::session::{CloseError, ExecuteOutcome, FetchedBatch};
use crate::shared::SessionShared;

/// A value a request replies with, and the [`SessionEvent`] it becomes.
///
/// Implemented once per reply shape, so the worker cannot answer a fetch with
/// an execute's event or forget which result a failure was about.
pub(crate) trait ReplyPayload: Send + Sized + 'static {
    /// What the reply is *about*, beyond the request id: the result that was
    /// fetched, the large object that was read, the operation that completed.
    /// `Copy`, so the failure path can build the same-shaped event a success
    /// would have (ADR-0003 A21).
    type Subject: Copy + Send + 'static;

    /// Builds the one event this reply produces.
    fn into_event(
        session: SessionId,
        request: RequestId,
        subject: Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent;
}

/// What one successful open produced, on its way to
/// [`SessionEvent::Opened`].
///
/// The open is a request like any other on the event path
/// (`docs/exec-plans/active/phase-1.md` §B3): it reserves a slot, it is
/// answered exactly once, and — this is the whole reason it is a
/// [`ReplyPayload`] rather than a bespoke path —
/// [`crate::SessionRegistry::abandon`] answers it by *dropping* the reply
/// channel, which emits `OpenFailed` through the same mechanism that covers
/// every other request (ADR-0002 E2).
#[derive(Debug)]
pub(crate) struct OpenedSession {
    pub(crate) connection: ConnectionId,
    pub(crate) cancel_kind: CancelKind,
    pub(crate) warnings: Vec<Warning>,
}

impl ReplyPayload for OpenedSession {
    type Subject = ();

    fn into_event(
        session: SessionId,
        request: RequestId,
        (): Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        match value {
            Ok(opened) => SessionEvent::Opened {
                session,
                request,
                connection: opened.connection,
                cancel_kind: opened.cancel_kind,
                warnings: opened.warnings,
            },
            Err(error) => SessionEvent::OpenFailed {
                session,
                request,
                error,
            },
        }
    }
}

impl ReplyPayload for ExecuteOutcome {
    type Subject = ();

    fn into_event(
        session: SessionId,
        request: RequestId,
        (): Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        SessionEvent::Executed {
            session,
            request,
            outcome: value,
        }
    }
}

impl ReplyPayload for FetchedBatch {
    type Subject = ResultId;

    fn into_event(
        session: SessionId,
        request: RequestId,
        result: Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        SessionEvent::Fetched {
            session,
            request,
            result,
            batch: value,
        }
    }
}

impl ReplyPayload for Vec<u8> {
    type Subject = LobHandle;

    fn into_event(
        session: SessionId,
        request: RequestId,
        lob: Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        SessionEvent::LobChunk {
            session,
            request,
            lob,
            bytes: value,
        }
    }
}

impl ReplyPayload for ServerOutputSetting {
    type Subject = ();

    fn into_event(
        session: SessionId,
        request: RequestId,
        (): Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        SessionEvent::ServerOutputConfigured {
            session,
            request,
            result: value,
        }
    }
}

impl ReplyPayload for () {
    type Subject = CompletedOperation;

    fn into_event(
        session: SessionId,
        request: RequestId,
        operation: Self::Subject,
        value: DbResult<Self>,
    ) -> SessionEvent {
        SessionEvent::Completed {
            session,
            request,
            operation,
            result: value,
        }
    }
}

enum Channel<T> {
    /// The per-request oneshot behind [`crate::Completion`].
    OneShot(Sender<DbResult<T>>),
    /// The shared event queue, through the session's emit lock.
    Event {
        shared: Arc<SessionShared>,
        session: SessionId,
        request: RequestId,
    },
}

/// Where one request's answer goes. See the module documentation.
pub(crate) struct ReplyTo<T: ReplyPayload> {
    channel: Option<Channel<T>>,
    subject: T::Subject,
}

impl<T: ReplyPayload> ReplyTo<T> {
    /// A reply the caller waits for with [`crate::Completion`].
    pub(crate) const fn one_shot(reply: Sender<DbResult<T>>, subject: T::Subject) -> Self {
        Self {
            channel: Some(Channel::OneShot(reply)),
            subject,
        }
    }

    /// A reply that becomes one [`SessionEvent`].
    pub(crate) const fn event(
        shared: Arc<SessionShared>,
        session: SessionId,
        request: RequestId,
        subject: T::Subject,
    ) -> Self {
        Self {
            channel: Some(Channel::Event {
                shared,
                session,
                request,
            }),
            subject,
        }
    }

    /// The request id, on the event path only. The worker uses it for
    /// [`SessionEvent::Executing`], which has no completion-path equivalent.
    pub(crate) const fn request(&self) -> Option<RequestId> {
        match &self.channel {
            Some(Channel::Event { request, .. }) => Some(*request),
            Some(Channel::OneShot(_)) | None => None,
        }
    }

    /// Answers the request. Reports whether the answer reached anyone: `false`
    /// means the caller dropped its [`crate::Completion`], so whatever this
    /// command registered has to be released on the worker thread instead of
    /// being handed over.
    pub(crate) fn answer(mut self, value: DbResult<T>) -> bool {
        let subject = self.subject;
        match self.channel.take() {
            Some(Channel::OneShot(reply)) => reply.send(value).is_ok(),
            Some(Channel::Event {
                shared,
                session,
                request,
            }) => {
                shared.emit_reply(T::into_event(session, request, subject, value));
                true
            }
            // Unreachable: `answer` consumes the only handle.
            None => false,
        }
    }
}

impl<T: ReplyPayload> Drop for ReplyTo<T> {
    fn drop(&mut self) {
        let subject = self.subject;
        if let Some(Channel::Event {
            shared,
            session,
            request,
        }) = self.channel.take()
        {
            let error = shared.terminal_error();
            shared.emit_reply(T::into_event(session, request, subject, Err(error)));
        }
    }
}

enum CloseChannel {
    OneShot(Sender<Result<(), CloseError>>),
    Event {
        shared: Arc<SessionShared>,
        session: SessionId,
        request: RequestId,
    },
}

/// [`ReplyTo`] for a close, whose answer is a [`CloseError`] rather than a
/// [`reldex_db_driver_api::DbError`] — three of its variants leave the session
/// open, which no ordinary failure can do.
pub(crate) struct CloseReplyTo {
    channel: Option<CloseChannel>,
}

impl CloseReplyTo {
    /// A close the caller waits for (`DatabaseSession::close`, and `Drop`).
    pub(crate) const fn one_shot(reply: Sender<Result<(), CloseError>>) -> Self {
        Self {
            channel: Some(CloseChannel::OneShot(reply)),
        }
    }

    /// A close that becomes one [`SessionEvent::SessionClosed`].
    pub(crate) const fn event(
        shared: Arc<SessionShared>,
        session: SessionId,
        request: RequestId,
    ) -> Self {
        Self {
            channel: Some(CloseChannel::Event {
                shared,
                session,
                request,
            }),
        }
    }

    /// Answers the close. Reports whether anyone received it.
    pub(crate) fn answer(mut self, result: Result<(), CloseError>) -> bool {
        match self.channel.take() {
            Some(CloseChannel::OneShot(reply)) => reply.send(result).is_ok(),
            Some(CloseChannel::Event {
                shared,
                session,
                request,
            }) => {
                shared.emit_reply(SessionEvent::SessionClosed {
                    session,
                    request,
                    result,
                });
                true
            }
            None => false,
        }
    }
}

impl Drop for CloseReplyTo {
    fn drop(&mut self) {
        if let Some(CloseChannel::Event {
            shared,
            session,
            request,
        }) = self.channel.take()
        {
            // A close is idempotent, and it stays idempotent when it loses a
            // race. Reaching here means the command never ran — the worker had
            // already gone — so the question is only *why* it had gone, and
            // that is exactly what `settled_close` answers, from the one
            // record of how the session ended. A close that really did end
            // this session cleanly makes this one a success; a session that
            // was lost or abandoned owes it the truth instead.
            let result = shared
                .settled_close()
                .unwrap_or_else(|| Err(shared.terminal_error()))
                .map_err(CloseError::Failed);
            shared.emit_reply(SessionEvent::SessionClosed {
                session,
                request,
                result,
            });
        }
    }
}
