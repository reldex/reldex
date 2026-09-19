//! `Completion` must never invent an answer it does not have.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use reldex_db_core::Statement;
use reldex_driver_mock::{Action, BlockGate, BlockSpec};

const BLOCKER: &str = "BEGIN long_running; END;";

/// Polling a completion that has not finished must hand it back, not consume
/// the reply.
///
/// `poll(&self)` used to `try_recv` and throw the receiver's state away, so a
/// second poll after a successful one reported "the session worker ended
/// without replying" — an error the worker never produced, about a session that
/// was perfectly healthy.
#[test]
fn polling_never_fabricates_an_answer_it_does_not_have() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(BLOCKER, Action::Block(BlockSpec::new(Arc::clone(&gate))));

    let session = support::open(&scenario);
    let completion = session.execute(Statement::new(BLOCKER));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    // Not finished: the completion comes back untouched, however many times it
    // is asked.
    let mut completion = completion;
    for _ in 0..5 {
        completion = match completion.poll() {
            Ok(value) => panic!("the statement is still blocked, yet poll answered: {value:?}"),
            Err(pending) => pending,
        };
    }

    gate.release();
    // Once it does finish, the answer arrives exactly once, and it is the real
    // one.
    let mut completion = completion;
    let answer = loop {
        match completion.poll() {
            Ok(value) => break value,
            Err(pending) => {
                completion = pending;
                std::thread::yield_now();
            }
        }
    };
    answer.expect("the blocked statement succeeded once released");
}

#[test]
fn wait_timeout_gives_up_on_the_wait_without_giving_up_on_the_request() {
    let scenario = support::scenario();
    let gate = BlockGate::new();
    scenario.on_sql(BLOCKER, Action::Block(BlockSpec::new(Arc::clone(&gate))));

    let session = support::open(&scenario);
    let completion = session.execute(Statement::new(BLOCKER));
    assert!(gate.wait_until_blocked(support::short_timeout()));

    let started = Instant::now();
    let Err(completion) = completion.wait_timeout(Duration::from_millis(50)) else {
        panic!("the statement is blocked, so the wait must time out");
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "it waited far longer than it was told"
    );

    // The request kept running: releasing it still completes normally.
    gate.release();
    completion
        .wait_timeout(support::short_timeout())
        .expect("the request was never cancelled by giving up on the wait")
        .expect("and it succeeded");
}
