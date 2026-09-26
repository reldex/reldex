//! [`GeneratedQuerySpec`] and [`GeneratedCursor`]: a query result produced
//! lazily, one fetch at a time, instead of replayed from a pre-built
//! [`crate::scenario::QueryPlan`].
//!
//! The mock's other queries hold their whole result in memory
//! (`crate::scenario::QueryPlan::rows`), which is fine for the small fixed
//! fixtures most tests need but cannot stand in for a **1,000,000-row**
//! result: materialising that many [`crate::scenario::ScriptValue`]s just to
//! prove a UI can scroll them defeats the point of the exercise
//! (`docs/exec-plans/active/phase-0-spike-results.md` S14; the upcoming S15
//! FFI/UI spike needs the mock to carry the same shape through `db-core`).
//!
//! # How laziness works
//!
//! [`GeneratedQuerySpec`] carries only parameters — a row count, a small list
//! of column generators, a seed and two optional latencies — never rows.
//! [`GeneratedCursor`] holds that spec, a `u64` cursor position and a couple
//! of flags; it has no field whose size depends on how many rows the result
//! has. Each [`Cursor::fetch_batch`] call computes the values for *only* the
//! rows in that batch, builds a [`RowBatch`] from them (reusing the exact
//! conversion [`crate::cursor::build_column`] a scripted query uses), and
//! returns it — nothing produced for a batch survives past it except the
//! advanced position. Memory is therefore `O(batch)`, never `O(rows)`.
//!
//! # How determinism works
//!
//! Every cell is a pure function of `(seed, row, column)`:
//! [`GeneratedQuerySpec::expected_cell`] computes it directly, with no cursor,
//! no fetch and no side effect, and [`GeneratedCursor`] computes the *same*
//! values through the *same* per-column generators when it actually builds a
//! batch. A test can therefore fetch to an arbitrary position — as a UI does
//! after scrolling — and check any cell against `expected_cell` without
//! re-reading the rows that came before it.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reldex_db_driver_api::{
    ColumnMetadata, ConnectionId, Cursor, DbError, DbResult, ErrorKind, Number, ResultSetId,
    RowBatch, SessionState, SqlType, Timestamp,
};

use crate::cursor::{build_column, build_metadata};
use crate::scenario::{ColumnSpec, Scenario, ScriptValue, TransactionEpoch};

/// Why a [`GeneratedQuerySpec`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeneratedQuerySpecError {
    /// The spec declared no columns, so there is nothing to fetch.
    NoColumns,
}

impl fmt::Display for GeneratedQuerySpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoColumns => f.write_str("a generated query must declare at least one column"),
        }
    }
}

impl std::error::Error for GeneratedQuerySpecError {}

/// One column of a [`GeneratedQuerySpec`]'s result, and how its values are
/// derived from the row number.
///
/// Deliberately a small, closed set rather than an arbitrary callback: a
/// callback could not promise [`GeneratedQuerySpec::expected_cell`] the same
/// value fetching actually produces, since nothing would stop it from
/// carrying hidden state. Each variant carries the column's name so a
/// caller-provided shape can rename columns without changing what they
/// generate.
#[derive(Debug, Clone)]
pub enum GeneratedColumn {
    /// `NUMBER`: the 1-based row number, `1..=rows`.
    Id(String),
    /// Deterministic `VARCHAR2(40)`-shaped text, in the style of
    /// `RPAD('row ' || n, 40, '.')`. At a fixed cadence the value is instead
    /// SQL NULL, Thai text, or text containing a non-BMP emoji — see the
    /// module documentation on [`GeneratedQuerySpec::s14_shape`].
    Name(String),
    /// `DATE`: strictly increasing with the row number, one day per row
    /// starting at 2026-01-01.
    Created(String),
}

impl GeneratedColumn {
    fn name(&self) -> &str {
        match self {
            Self::Id(name) | Self::Name(name) | Self::Created(name) => name,
        }
    }

    const fn sql_type(&self) -> SqlType {
        match self {
            Self::Id(_) => SqlType::Number,
            Self::Name(_) => SqlType::VARCHAR,
            Self::Created(_) => SqlType::Date,
        }
    }

    fn spec(&self) -> ColumnSpec {
        let spec = ColumnSpec::new(self.name(), self.sql_type());
        match self {
            // `VARCHAR2(40 CHAR)` in AL32UTF8, as Oracle describes it: up to
            // four bytes a character. ADR-0004's S14 row width assumes it.
            Self::Name(_) => spec.with_max_size_bytes(NAME_MAX_SIZE_BYTES),
            Self::Id(_) | Self::Created(_) => spec,
        }
    }

    /// The value at the given 1-based row number.
    fn value_for(&self, row_number: u64, seed: u64) -> ScriptValue {
        match self {
            Self::Id(_) => ScriptValue::Number(Number::from(row_number)),
            Self::Name(_) => generated_name(row_number, seed),
            Self::Created(_) => ScriptValue::Timestamp(created_for(row_number)),
        }
    }
}

/// The byte width [`GeneratedColumn::Name`] declares: 40 characters of up to
/// four bytes each.
const NAME_MAX_SIZE_BYTES: u32 = 160;

/// Every `n`th row (1-based) is SQL NULL in [`GeneratedColumn::Name`].
const NULL_CADENCE: u64 = 100;
/// Every `n`th row not already NULL carries a non-BMP emoji.
const EMOJI_CADENCE: u64 = 25;
/// Every `n`th row not already NULL or emoji is Thai text.
const THAI_CADENCE: u64 = 10;

const THAI_WORDS: [&str; 4] = ["ข้อมูล", "ทดสอบ", "แถว", "ฐานข้อมูล"];
const EMOJI_GLYPHS: [&str; 4] = ["🚀", "🎉", "🐍", "🧩"];

/// [`GeneratedColumn::Name`]'s value for 1-based `row_number`.
///
/// Cadence is checked from the narrowest condition down (NULL, then emoji,
/// then Thai) so a row divisible by more than one cadence has one unambiguous
/// outcome: row 100 is divisible by 10 and 25 too, and is NULL.
fn generated_name(row_number: u64, seed: u64) -> ScriptValue {
    if row_number % NULL_CADENCE == 0 {
        return ScriptValue::Null;
    }
    if row_number % EMOJI_CADENCE == 0 {
        let index = pick(row_number, seed, EMOJI_GLYPHS.len());
        return ScriptValue::Text(pad40(&format!("row {row_number} {}", EMOJI_GLYPHS[index])));
    }
    if row_number % THAI_CADENCE == 0 {
        let index = pick(row_number, seed, THAI_WORDS.len());
        return ScriptValue::Text(pad40(&format!("{} {row_number}", THAI_WORDS[index])));
    }
    ScriptValue::Text(pad40(&format!("row {row_number}")))
}

/// A deterministic index in `0..len`, mixing the seed into the choice so two
/// seeds can pick different words for the same row while staying a pure
/// function of both.
fn pick(row_number: u64, seed: u64, len: usize) -> usize {
    let len = u64::try_from(len).unwrap_or(1);
    usize::try_from(row_number.wrapping_add(seed) % len.max(1)).unwrap_or(0)
}

/// Pads `text` with `.` to 40 **characters**, mirroring the S14 spike's
/// `RPAD('row ' || n, 40, '.')`. Text already at or past 40 characters (a
/// wide Thai or emoji row) is left as is, exactly as `RPAD` would leave it.
fn pad40(text: &str) -> String {
    let mut padded = text.to_owned();
    let len = padded.chars().count();
    if len < 40 {
        padded.extend(std::iter::repeat_n('.', 40 - len));
    }
    padded
}

/// The first generated row's date: 2026-01-01, matching the base date the
/// S14 spike used.
const BASE_YEAR: i64 = 2026;
const BASE_MONTH: i64 = 1;
const BASE_DAY: i64 = 1;

/// [`GeneratedColumn::Created`]'s value for 1-based `row_number`: the base
/// date plus `row_number` days, so row 1 is one day after the base date.
fn created_for(row_number: u64) -> Timestamp {
    let base_days = days_from_civil(BASE_YEAR, BASE_MONTH, BASE_DAY);
    let offset = i64::try_from(row_number).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(base_days.saturating_add(offset));
    // Every row number this crate is asked to generate in practice (up to a
    // few million) lands centuries before the year-9999 ceiling, so this
    // never actually falls back; the fallback exists so an absurd `rows`
    // value reports a valid, if meaningless, date instead of panicking.
    let year = year.clamp(1, 9999);
    Timestamp::from_source(
        i16::try_from(year).unwrap_or(9999),
        u8::try_from(month).unwrap_or(1),
        u8::try_from(day).unwrap_or(1),
        0,
        0,
        0,
    )
    .unwrap_or_else(|_| {
        Timestamp::from_source(9999, 12, 31, 0, 0, 0).expect("2026-12-31 is always a valid date")
    })
}

/// Proleptic-Gregorian days since 1970-01-01 for a civil date, valid for
/// `year >= 1`.
///
/// This is Howard Hinnant's `days_from_civil` algorithm (public domain,
/// <https://howardhinnant.github.io/date_algorithms.html>), used here rather
/// than a day-by-day loop so [`GeneratedQuerySpec::expected_cell`] can compute
/// an arbitrary row's date in O(1) instead of O(row). Every date this module
/// produces is after the 1582 Gregorian reform, where this crate's own
/// [`Timestamp`] validation already uses the plain Gregorian leap rule, so the
/// two agree without needing the historical Julian correction
/// `reldex_db_driver_api::value::temporal` applies before 1582.
const fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A lazily produced query result: total row count, column shape, an optional
/// seed-mixed variation and optional latencies, but never the rows
/// themselves.
///
/// Build the default shape with [`GeneratedQuerySpec::s14_shape`] — `ID
/// NUMBER`, `NAME VARCHAR2(40)`, `CREATED DATE`, matching
/// `crates/drivers/oracle-thin/tests/s14_large_result.rs` as far as this
/// crate's vendor-neutral types allow — or [`GeneratedQuerySpec::new`] for a
/// caller-provided list of [`GeneratedColumn`]s.
///
/// ```
/// use reldex_driver_mock::{Action, GeneratedQuerySpec, Scenario};
///
/// let spec = GeneratedQuerySpec::s14_shape(1_000_000, 42);
/// assert_eq!(spec.rows(), 1_000_000);
///
/// // Deterministic and checkable without fetching anything.
/// assert_eq!(spec.expected_cell(999_999, 4), None, "only 3 columns exist");
/// let id = spec.expected_cell(0, 0).expect("row 0 exists");
/// assert_eq!(id, reldex_driver_mock::ScriptValue::from(1_i64));
///
/// let scenario = Scenario::new();
/// scenario.on_sql("SELECT * FROM big", Action::GeneratedQuery(spec));
/// ```
#[derive(Debug, Clone)]
pub struct GeneratedQuerySpec {
    rows: u64,
    columns: Vec<GeneratedColumn>,
    seed: u64,
    per_fetch_latency: Option<Duration>,
    first_batch_latency: Option<Duration>,
}

impl GeneratedQuerySpec {
    /// Declares a lazily produced result of `rows` rows shaped by `columns`,
    /// varied by `seed`.
    ///
    /// # Errors
    ///
    /// [`GeneratedQuerySpecError::NoColumns`] if `columns` is empty: a result
    /// with no columns has nothing to fetch, and every downstream consumer
    /// (`RowBatch`, a UI's column headers) would have to special-case it.
    pub fn new(
        rows: u64,
        columns: Vec<GeneratedColumn>,
        seed: u64,
    ) -> Result<Self, GeneratedQuerySpecError> {
        if columns.is_empty() {
            return Err(GeneratedQuerySpecError::NoColumns);
        }
        Ok(Self {
            rows,
            columns,
            seed,
            per_fetch_latency: None,
            first_batch_latency: None,
        })
    }

    /// The shape `crates/drivers/oracle-thin/tests/s14_large_result.rs` used
    /// for its large-result spike: `ID NUMBER` (`1..=rows`), `NAME
    /// VARCHAR2(40)` (deterministic text, with NULL every 100th row, a
    /// non-BMP emoji every 25th row not already NULL, and Thai text every
    /// 10th row not already NULL or emoji), and `CREATED DATE` (one day per
    /// row, starting 2026-01-01).
    #[must_use]
    pub fn s14_shape(rows: u64, seed: u64) -> Self {
        Self::new(
            rows,
            vec![
                GeneratedColumn::Id("ID".to_owned()),
                GeneratedColumn::Name("NAME".to_owned()),
                GeneratedColumn::Created("CREATED".to_owned()),
            ],
            seed,
        )
        .expect("the S14 shape always declares three columns")
    }

    /// Adds a simulated per-fetch round-trip cost.
    ///
    /// Applied inside [`Cursor::fetch_batch`] on *every* call, including the
    /// first, as a plain [`std::thread::sleep`] — so it occupies the calling
    /// thread exactly as a real network round trip would occupy a session's
    /// worker thread, which is what lets a test prove a slow fetch on one
    /// session does not stall another (see the crate tests).
    #[must_use]
    pub const fn with_per_fetch_latency(mut self, latency: Duration) -> Self {
        self.per_fetch_latency = Some(latency);
        self
    }

    /// Adds simulated latency between `execute` and the first row becoming
    /// available, on top of any [`GeneratedQuerySpec::with_per_fetch_latency`]
    /// — modelling the extra cost a first round trip often has (parsing,
    /// connection warm-up) beyond the steady-state per-fetch cost.
    ///
    /// Applied as a sleep at the start of the *first* `fetch_batch` call
    /// only, since a mock query already returns its cursor from `execute`
    /// without fetching anything.
    #[must_use]
    pub const fn with_first_batch_latency(mut self, latency: Duration) -> Self {
        self.first_batch_latency = Some(latency);
        self
    }

    /// The total number of rows the result will produce.
    #[must_use]
    pub const fn rows(&self) -> u64 {
        self.rows
    }

    /// The column shape, in select-list order.
    #[must_use]
    pub fn columns(&self) -> &[GeneratedColumn] {
        &self.columns
    }

    /// The seed mixed into cell values that vary beyond the row number (see
    /// [`GeneratedColumn::Name`]).
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// The per-fetch latency, if set.
    #[must_use]
    pub const fn per_fetch_latency(&self) -> Option<Duration> {
        self.per_fetch_latency
    }

    /// The first-batch latency, if set.
    #[must_use]
    pub const fn first_batch_latency(&self) -> Option<Duration> {
        self.first_batch_latency
    }

    /// The value at `(row, column)`, as a pure function of `(seed, row,
    /// column)` — no cursor, no fetch, no side effect.
    ///
    /// `row` is 0-based across the **whole** result, not per-batch, so a test
    /// can check a cell reached after scrolling to an arbitrary position
    /// without re-fetching from the start. Returns `None` if `row >= rows` or
    /// `column` is out of range. [`GeneratedCursor::fetch_batch`] computes
    /// every cell it returns through this same column-generator logic, so the
    /// two always agree.
    #[must_use]
    pub fn expected_cell(&self, row: u64, column: usize) -> Option<ScriptValue> {
        if row >= self.rows {
            return None;
        }
        let generator = self.columns.get(column)?;
        Some(generator.value_for(row + 1, self.seed))
    }

    pub(crate) fn column_specs(&self) -> Vec<ColumnSpec> {
        self.columns.iter().map(GeneratedColumn::spec).collect()
    }
}

/// A forward-only cursor that computes each batch's rows on demand from a
/// [`GeneratedQuerySpec`] instead of slicing a pre-built result.
///
/// Holds no row data: only the spec (parameters, not rows), the position of
/// the next row to produce, and the same connection-lifecycle plumbing
/// [`crate::cursor::MockCursor`] uses (a closed flag and a transaction epoch,
/// so this cursor honours the same connection-close and
/// commit/rollback-invalidation rules — ADR-0002 D2 — as every other mock
/// cursor). `fetch_batch` therefore costs `O(batch)`, not `O(rows)`, in both
/// time and memory.
pub struct GeneratedCursor {
    id: ResultSetId,
    connection_id: ConnectionId,
    metadata: Vec<ColumnMetadata>,
    spec: GeneratedQuerySpec,
    /// 0-based index of the next row `fetch_batch` will produce.
    next_row: u64,
    exhausted: bool,
    first_fetch_done: bool,
    /// See [`crate::cursor::MockCursor`]'s field of the same purpose: once
    /// set, every later `fetch_batch` reports it again rather than resuming.
    failed: Option<(ErrorKind, String, SessionState)>,
    scenario: Arc<Scenario>,
    closed: Arc<AtomicBool>,
    epoch: TransactionEpoch,
}

impl GeneratedCursor {
    pub(crate) fn new(
        connection_id: ConnectionId,
        spec: GeneratedQuerySpec,
        scenario: Arc<Scenario>,
        closed: Arc<AtomicBool>,
        epoch: TransactionEpoch,
    ) -> Self {
        let metadata = build_metadata(&spec.column_specs());
        Self {
            id: ResultSetId::allocate(),
            connection_id,
            metadata,
            spec,
            next_row: 0,
            exhausted: false,
            first_fetch_done: false,
            failed: None,
            scenario,
            closed,
            epoch,
        }
    }

    /// Remembers enough of `error` to report it again; see
    /// [`crate::cursor::MockCursor::fail`].
    fn fail(&mut self, error: DbError) -> DbError {
        self.exhausted = true;
        self.failed = Some((
            error.kind(),
            error.message().to_owned(),
            error.session_state(),
        ));
        error
    }

    fn repeat_failure(&self) -> Option<DbError> {
        self.failed.as_ref().map(|(kind, message, session_state)| {
            DbError::new(*kind, message.clone()).with_session_state(*session_state)
        })
    }
}

impl fmt::Debug for GeneratedCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedCursor")
            .field("id", &self.id)
            .field("connection_id", &self.connection_id)
            .field("rows", &self.spec.rows())
            .field("next_row", &self.next_row)
            .field("exhausted", &self.exhausted)
            .finish_non_exhaustive()
    }
}

impl Drop for GeneratedCursor {
    /// Releasing a generated cursor is driver work too; see
    /// [`crate::cursor::MockCursor`]'s `Drop`.
    fn drop(&mut self) {
        self.scenario.record_thread(self.connection_id);
    }
}

impl Cursor for GeneratedCursor {
    fn id(&self) -> ResultSetId {
        self.id
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn columns(&self) -> &[ColumnMetadata] {
        &self.metadata
    }

    fn fetch_batch(&mut self, max_rows: NonZeroUsize) -> DbResult<RowBatch> {
        self.scenario.record_thread(self.connection_id);
        if let Some(error) = self.repeat_failure() {
            return Err(error);
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(self.fail(DbError::connection_closed("cursor")));
        }
        if let Some(error) = self.epoch.check("cursor") {
            return Err(self.fail(error));
        }
        if self.exhausted {
            return Ok(RowBatch::empty());
        }

        // Simulated latency happens here, inside the blocking call, so it
        // occupies the caller's thread exactly as a real round trip would.
        let is_first_fetch = !self.first_fetch_done;
        self.first_fetch_done = true;
        if is_first_fetch && let Some(latency) = self.spec.first_batch_latency() {
            std::thread::sleep(latency);
        }
        if let Some(latency) = self.spec.per_fetch_latency() {
            std::thread::sleep(latency);
        }

        let total = self.spec.rows();
        let remaining = total - self.next_row;
        let max_rows_u64 = u64::try_from(max_rows.get()).unwrap_or(u64::MAX);
        let take = remaining.min(max_rows_u64);
        if take == 0 {
            self.exhausted = true;
            return Ok(RowBatch::empty());
        }
        let take_usize = usize::try_from(take).unwrap_or(usize::MAX);

        // The only per-call allocation: `take` rows (at most `max_rows`), row
        // major, converted below into the column-oriented shape the batch
        // needs and then dropped at the end of this call. Nothing here scales
        // with `total`.
        let mut generated_rows: Vec<Vec<ScriptValue>> = Vec::with_capacity(take_usize);
        for offset in 0..take {
            let row_number = self.next_row + offset + 1; // 1-based
            generated_rows.push(
                self.spec
                    .columns()
                    .iter()
                    .map(|generator| generator.value_for(row_number, self.spec.seed()))
                    .collect(),
            );
        }

        let column_specs = self.spec.column_specs();
        let columns = (0..column_specs.len())
            .map(|index| {
                build_column(
                    &column_specs[index],
                    &generated_rows,
                    index,
                    self.connection_id,
                    &self.scenario,
                    &self.closed,
                    &self.epoch,
                )
            })
            .collect::<DbResult<Vec<_>>>()?;

        self.next_row += take;
        if self.next_row >= total {
            self.exhausted = true;
        }
        RowBatch::new(columns)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Releases the cursor. Always `Ok(())`; see
    /// [`crate::cursor::MockCursor::close`].
    fn close(self: Box<Self>) -> DbResult<()> {
        self.scenario.record_thread(self.connection_id);
        self.scenario.record_cursor_closed();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_with_no_columns_is_rejected() {
        let error = GeneratedQuerySpec::new(10, Vec::new(), 0)
            .expect_err("an empty column list must be refused");
        assert_eq!(error, GeneratedQuerySpecError::NoColumns);
    }

    #[test]
    fn expected_cell_is_out_of_range_safe() {
        let spec = GeneratedQuerySpec::s14_shape(3, 0);
        assert!(spec.expected_cell(3, 0).is_none(), "row out of range");
        assert!(spec.expected_cell(0, 3).is_none(), "column out of range");
        assert!(spec.expected_cell(0, 0).is_some());
    }

    #[test]
    fn the_name_cadence_hits_null_emoji_and_thai_in_priority_order() {
        // Row 100 is divisible by 10, 25 and 100: NULL wins.
        assert_eq!(generated_name(100, 0), ScriptValue::Null);
        // Row 25 is divisible by 25 but not 100: emoji.
        assert!(
            matches!(generated_name(25, 0), ScriptValue::Text(text) if EMOJI_GLYPHS.iter().any(|glyph| text.contains(glyph)))
        );
        // Row 10 is divisible by 10 but not 25 or 100: Thai.
        assert!(
            matches!(generated_name(10, 0), ScriptValue::Text(text) if THAI_WORDS.iter().any(|word| text.contains(word)))
        );
        // Row 7 matches no cadence: plain padded text.
        assert_eq!(
            generated_name(7, 0),
            ScriptValue::Text("row 7".to_owned() + &".".repeat(35))
        );
    }

    #[test]
    fn created_dates_increase_one_day_per_row() {
        let first = created_for(1);
        let second = created_for(2);
        assert_eq!((first.year(), first.month(), first.day()), (2026, 1, 2));
        assert_eq!((second.year(), second.month(), second.day()), (2026, 1, 3));
    }

    #[test]
    fn civil_day_conversion_round_trips() {
        for (y, m, d) in [(2026, 1, 1), (2026, 2, 28), (2028, 2, 29), (4765, 3, 4)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
    }
}
