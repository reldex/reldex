//! Many sessions, one queue: per-session FIFO holds and every accepted
//! request is answered exactly once, globally
//! (`docs/exec-plans/active/phase-1.md` §B2 rules 1, 2 and 5; §C M2's exit
//! gate, "8 sessions … exactly one `Terminal` each and exactly one reply per
//! request").
//!
//! The outcome is deterministic: the test knows exactly how many replies and
//! how many `Terminal`s must arrive, and asserts the whole multiset. What it
//! deliberately does not assert is any *cross-session* order, because rule 5
//! does not promise one.

mod support;

use std::collections::HashMap;
use std::thread;

use reldex_db_core::{
    CloseDisposition, RequestId, SessionEvent, SessionId, SessionLifecycle, Statement,
};
use reldex_driver_mock::{Action, ColumnSpec, QuerySource, ScriptValue};

const SESSIONS: usize = 8;
const REQUESTS_PER_SESSION: u64 = 40;

#[test]
fn many_sessions_fan_in_with_per_session_fifo_and_global_exactly_once() {
    let scenario = support::scenario();
    for index in 0..SESSIONS {
        let table = format!("t{index}");
        scenario.on_sql(
            format!("INSERT INTO {table} VALUES ('row')"),
            Action::Dml {
                rows_affected: 1,
                insert: Some((table.clone(), vec![ScriptValue::from("row")])),
            },
        );
        scenario.on_sql(
            format!("SELECT * FROM {table}"),
            Action::query(QuerySource::Table {
                table,
                columns: vec![ColumnSpec::new(
                    "NAME",
                    reldex_db_driver_api::SqlType::VARCHAR,
                )],
            }),
        );
    }

    let (sessions, queue) = support::open_events_fan_in(&scenario, SESSIONS);
    let ids: Vec<SessionId> = sessions
        .iter()
        .map(reldex_db_core::DatabaseSession::id)
        .collect();

    // Every session is driven from its own thread, so the only thing ordering
    // the queue is the workers racing each other into it.
    thread::scope(|scope| {
        for (index, session) in sessions.iter().enumerate() {
            scope.spawn(move || {
                let table = format!("t{index}");
                for request in 1..=REQUESTS_PER_SESSION {
                    let statement = if request.is_multiple_of(2) {
                        Statement::new(format!("INSERT INTO {table} VALUES ('row')"))
                    } else {
                        Statement::new(format!("SELECT * FROM {table}"))
                    };
                    session
                        .submit_execute(RequestId(request), statement)
                        .expect("accepted");
                }
                session
                    .submit_close(
                        RequestId(REQUESTS_PER_SESSION + 1),
                        Some(CloseDisposition::Rollback),
                    )
                    .expect("accepted");
            });
        }
    });

    // One reply per request, one close reply per session, one Terminal per
    // session.
    let expected_replies = SESSIONS * (REQUESTS_PER_SESSION as usize + 1);
    let seen = support::drain_until(&queue, |seen| {
        let replies = seen.iter().filter(|event| event.is_reply()).count();
        let terminals = seen.iter().filter(|event| event.is_terminal()).count();
        replies >= expected_replies && terminals >= SESSIONS
    });

    let mut per_session: HashMap<SessionId, Vec<u64>> = HashMap::new();
    let mut terminals: HashMap<SessionId, usize> = HashMap::new();
    for event in &seen {
        if event.is_reply() {
            let request = event.request().expect("a reply names its request");
            per_session
                .entry(event.session())
                .or_default()
                .push(request.0);
        }
        if let SessionEvent::Terminal {
            session, lifecycle, ..
        } = event
        {
            *terminals.entry(*session).or_default() += 1;
            assert_eq!(
                *lifecycle,
                SessionLifecycle::Closed,
                "these sessions were closed, not lost"
            );
        }
    }

    assert_eq!(
        per_session.len(),
        SESSIONS,
        "every session must appear in the queue"
    );
    for id in &ids {
        let replies = per_session.get(id).expect("this session's replies");
        assert_eq!(
            *replies,
            (1..=REQUESTS_PER_SESSION + 1).collect::<Vec<_>>(),
            "per-session FIFO, and exactly one reply per request, for {id}"
        );
        assert_eq!(
            terminals.get(id).copied(),
            Some(1),
            "exactly one Terminal for {id}"
        );
    }

    // And rule 5: the queue really did interleave, rather than emptying one
    // session at a time. Asserted as a property of *this* run only if it
    // happened — the rule promises freedom, not interleaving — so the check is
    // that nothing here depends on grouping.
    let extra_replies = seen.iter().filter(|event| event.is_reply()).count();
    assert_eq!(
        extra_replies, expected_replies,
        "no request is answered twice, across every session"
    );
}
