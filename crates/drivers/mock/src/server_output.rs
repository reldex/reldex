//! Scriptable server output — the mock's `DBMS_OUTPUT` (ADR-0002 amendment T).
//!
//! It behaves like the real thing in the three ways `db-core` depends on:
//!
//! - **Output is buffered only while it is enabled.** A statement scripted to
//!   print writes nothing on a connection that never enabled output, exactly as
//!   Oracle discards `PUT_LINE` when `DBMS_OUTPUT` is off — so a test cannot
//!   pass by accident on a session that forgot to enable.
//! - **A statement prints before it finishes**, so a statement scripted to
//!   fail still leaves its lines behind (a PL/SQL block that prints and then
//!   raises).
//! - **Disabling discards the buffer**, as `DBMS_OUTPUT.DISABLE` does.
//!
//! What a test controls on top: a failing `set`/`take`, a `take` that parks on a
//! [`BlockGate`] (for "abandon during a drain"), and how many times each was
//! called ([`ServerOutputCounts`]) — the counters are what prove "no round trip
//! when the pane is off".

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};

use reldex_db_driver_api::{DbError, DbResult, ServerOutputChunk, ServerOutputSetting};

use crate::scenario::{Behavior, BlockGate, Matcher, Scenario, ScriptedError};

/// How many times the server-output calls reached the mock.
///
/// Counted when the call **arrives**, before any scripted failure or block is
/// applied, so a call that failed still counts: the claim these back is "no
/// call was made", and a failed call was made.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ServerOutputCounts {
    /// Calls to `DatabaseConnection::set_server_output`.
    pub sets: usize,
    /// Calls to `DatabaseConnection::take_server_output`.
    pub takes: usize,
}

/// The scenario-wide script; one per [`Scenario`].
#[derive(Default)]
pub(crate) struct ServerOutputScript {
    state: Mutex<ScriptState>,
}

struct ScriptState {
    /// Lines a statement writes when it runs, by statement text, with how
    /// many of those lines are scripted to count as invalid UTF-8 (the
    /// trailing lines of the list, by convention — see
    /// [`Scenario::on_sql_output_with_invalid_utf8_lines`]).
    outputs: Vec<(Matcher, Vec<String>, u32)>,
    set: Behavior,
    take: Behavior,
    /// Parks every `take` until released.
    take_gate: Option<Arc<BlockGate>>,
    counts: ServerOutputCounts,
}

impl Default for ScriptState {
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            set: Behavior::Succeed,
            take: Behavior::Succeed,
            take_gate: None,
            counts: ServerOutputCounts::default(),
        }
    }
}

impl ServerOutputScript {
    fn lock(&self) -> MutexGuard<'_, ScriptState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Scenario {
    /// Scripts `lines` as the server output written by every statement whose
    /// trimmed text equals `sql`.
    ///
    /// Independent of the statement's [`crate::Action`]: the lines are written
    /// when the statement starts, so a statement scripted with
    /// [`crate::Action::Fail`] still leaves them in the buffer — the "print,
    /// then raise" block. They are buffered only on a connection that has
    /// enabled output, as on a real server.
    pub fn on_sql_output(&self, sql: impl Into<String>, lines: Vec<String>) {
        self.server_output
            .lock()
            .outputs
            .push((Matcher::Exact(sql.into()), lines, 0));
    }

    /// [`Scenario::on_sql_output`], plus `invalid_utf8_lines` of the scripted
    /// `lines` (the last `invalid_utf8_lines` of them) reported as if they had
    /// arrived on the wire as invalid UTF-8 — a driver's
    /// [`reldex_db_driver_api::ServerOutputChunk::invalid_utf8_lines`], scripted
    /// rather than produced by real corrupt bytes, so `db-core`'s handling of
    /// the count can be tested without a live, misbehaving database.
    ///
    /// The mock never actually corrupts the line text (it stores `String`,
    /// always valid UTF-8); what it fakes is only the count a real driver
    /// would report for those lines. The marker travels with each scripted
    /// line, so it survives however
    /// [`reldex_db_driver_api::DatabaseConnection::take_server_output`] happens
    /// to chunk them: a read's
    /// [`reldex_db_driver_api::ServerOutputChunk::invalid_utf8_lines`] is the
    /// number of *its own* lines that were marked, whichever chunk they land
    /// in.
    pub fn on_sql_output_with_invalid_utf8_lines(
        &self,
        sql: impl Into<String>,
        lines: Vec<String>,
        invalid_utf8_lines: u32,
    ) {
        debug_assert!(
            u64::from(invalid_utf8_lines) <= lines.len() as u64,
            "cannot mark more lines invalid than the statement prints"
        );
        self.server_output.lock().outputs.push((
            Matcher::Exact(sql.into()),
            lines,
            invalid_utf8_lines,
        ));
    }

    /// Makes every future `set_server_output` fail with `error`.
    pub fn fail_set_server_output(&self, error: ScriptedError) {
        self.server_output.lock().set = Behavior::Fail(error);
    }

    /// Makes every future `set_server_output` succeed again.
    pub fn allow_set_server_output(&self) {
        self.server_output.lock().set = Behavior::Succeed;
    }

    /// Makes every future `take_server_output` fail with `error`, taking
    /// nothing out of the buffer.
    ///
    /// With [`reldex_db_driver_api::SessionState::Lost`] this is the drain
    /// that discovers the connection is gone.
    pub fn fail_take_server_output(&self, error: ScriptedError) {
        self.server_output.lock().take = Behavior::Fail(error);
    }

    /// Makes every future `take_server_output` succeed again.
    pub fn allow_take_server_output(&self) {
        self.server_output.lock().take = Behavior::Succeed;
    }

    /// Parks every future `take_server_output` on `gate` until it is
    /// released — the drain-side twin of [`crate::Action::Block`].
    ///
    /// A cancel does not release it: a drain on the primary driver cannot be
    /// interrupted either (ADR-0001 C1), which is the case "abandon during a
    /// drain must not hang" is about.
    pub fn block_take_server_output(&self, gate: Arc<BlockGate>) {
        self.server_output.lock().take_gate = Some(gate);
    }

    /// Stops parking `take_server_output`.
    pub fn unblock_take_server_output(&self) {
        self.server_output.lock().take_gate = None;
    }

    /// How many times the server-output calls were made.
    #[must_use]
    pub fn server_output_counts(&self) -> ServerOutputCounts {
        self.server_output.lock().counts
    }

    fn scripted_output(&self, sql: &str) -> Option<(Vec<String>, u32)> {
        self.server_output
            .lock()
            .outputs
            .iter()
            .find(|(matcher, ..)| matcher.matches(sql))
            .map(|(_, lines, invalid_utf8_lines)| (lines.clone(), *invalid_utf8_lines))
    }

    fn record_set(&self) -> DbResult<()> {
        let mut state = self.server_output.lock();
        state.counts.sets += 1;
        state.set.apply()
    }

    fn record_take(&self) -> DbResult<()> {
        let gate = {
            let mut state = self.server_output.lock();
            state.counts.takes += 1;
            state.take_gate.clone()
        };
        // Parked outside the lock, so a test can still read the counters.
        if let Some(gate) = gate {
            let _ = gate.park(None, false);
        }
        self.server_output.lock().take.apply()
    }
}

/// One connection's server-side output buffer.
///
/// Each buffered line carries whether it was scripted as invalid UTF-8, so
/// [`ConnectionOutput::take`] can report the right count for *its own* chunk
/// regardless of how the buffer happens to be split across reads.
#[derive(Default)]
pub(crate) struct ConnectionOutput {
    setting: ServerOutputSetting,
    lines: VecDeque<(String, bool)>,
}

impl ConnectionOutput {
    /// A statement is starting: buffer what it is scripted to print, if output
    /// is on.
    pub(crate) fn statement_started(&mut self, scenario: &Scenario, sql: &str) {
        if !self.setting.is_enabled() {
            return;
        }
        if let Some((lines, invalid_utf8_lines)) = scenario.scripted_output(sql) {
            let total = lines.len();
            let first_invalid = total.saturating_sub(invalid_utf8_lines as usize);
            self.lines.extend(
                lines
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| (line, index >= first_invalid)),
            );
        }
    }

    pub(crate) fn set(
        &mut self,
        scenario: &Scenario,
        setting: ServerOutputSetting,
    ) -> DbResult<ServerOutputSetting> {
        scenario.record_set()?;
        if !setting.is_enabled() {
            // `DBMS_OUTPUT.DISABLE` purges the buffer; so does this.
            self.lines.clear();
        }
        self.setting = setting;
        Ok(setting)
    }

    pub(crate) fn take(
        &mut self,
        scenario: &Scenario,
        max_lines: NonZeroUsize,
        max_bytes: NonZeroUsize,
    ) -> DbResult<ServerOutputChunk> {
        scenario.record_take()?;
        let mut lines: Vec<Box<str>> = Vec::new();
        let mut bytes = 0_usize;
        let mut invalid_utf8_lines = 0_u32;
        while lines.len() < max_lines.get() {
            let Some((next, _)) = self.lines.front() else {
                break;
            };
            // At least one line per chunk, however long: a line is never
            // split (the contract's rule for `max_bytes`).
            if !lines.is_empty() && bytes.saturating_add(next.len()) > max_bytes.get() {
                break;
            }
            bytes = bytes.saturating_add(next.len());
            if let Some((line, marked_invalid)) = self.lines.pop_front() {
                if marked_invalid {
                    invalid_utf8_lines += 1;
                }
                lines.push(line.into_boxed_str());
            }
        }
        let drained = self.lines.is_empty();
        let chunk = ServerOutputChunk::new(lines, drained);
        Ok(if invalid_utf8_lines > 0 {
            chunk.with_invalid_utf8_lines(invalid_utf8_lines)
        } else {
            chunk
        })
    }
}

/// The error a connection without the capability answers with.
pub(crate) fn unsupported() -> DbError {
    DbError::unsupported("server output")
}
