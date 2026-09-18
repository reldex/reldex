//! Statement text, bind parameters and per-call execution options
//! (ADR-0002 D6).
//!
//! A [`Statement`] is literal SQL or PL/SQL text plus its binds and the few
//! options a driver must know *before* it executes. The contract does not parse
//! the text, split it, or decide what kind of statement it is: that is the
//! server's job, and a worksheet must be able to submit whatever the user typed
//! (`SPEC.md` §15 keeps script splitting out of this layer). What kind of
//! statement it turned out to be comes back in
//! [`ExecutionOutcome::statement_kind`](crate::ExecutionOutcome::statement_kind).
//!
//! A `Statement` is [`Clone`], so the same prepared call can be executed more
//! than once. That is why bind inputs are [`crate::BindValue`] rather than
//! [`crate::Value`].

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::types::SqlType;
use crate::value::BindValue;

/// Which way a bind parameter carries data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindDirection {
    /// Supplied by the caller, not written back.
    In,
    /// Written back by the server, not supplied.
    Out,
    /// Supplied by the caller and written back by the server.
    InOut,
}

/// What an OUT or IN OUT bind expects back.
///
/// The declared type is required: a server cannot always infer the type or size
/// of an output placeholder, and guessing is how truncation bugs happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutBindSpec {
    sql_type: SqlType,
    max_size_bytes: Option<u32>,
}

impl OutBindSpec {
    /// Declares an output of the given type.
    #[must_use]
    pub const fn new(sql_type: SqlType) -> Self {
        Self {
            sql_type,
            max_size_bytes: None,
        }
    }

    /// Declares the maximum size the driver should allocate for the output.
    #[must_use]
    pub const fn with_max_size_bytes(mut self, max_size_bytes: u32) -> Self {
        self.max_size_bytes = Some(max_size_bytes);
        self
    }

    /// The declared type.
    #[must_use]
    pub const fn sql_type(self) -> SqlType {
        self.sql_type
    }

    /// The declared maximum size in bytes, if any.
    #[must_use]
    pub const fn max_size_bytes(self) -> Option<u32> {
        self.max_size_bytes
    }
}

/// One bind parameter.
///
/// Input values are [`BindValue`] — plain, cloneable data — so a bound statement
/// can be executed more than once. A LOB locator or a nested cursor cannot be
/// written here at all, which replaces a rule the contract previously stated in
/// prose and could not enforce.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Bind {
    /// A value supplied to the server.
    In(BindValue),
    /// A placeholder the server fills in.
    Out(OutBindSpec),
    /// A value supplied to the server and overwritten by it.
    InOut {
        /// The value supplied.
        value: BindValue,
        /// What is expected back.
        spec: OutBindSpec,
    },
}

impl Bind {
    /// An IN bind.
    pub fn input(value: impl Into<BindValue>) -> Self {
        Self::In(value.into())
    }

    /// An OUT bind of the given type.
    #[must_use]
    pub const fn output(sql_type: SqlType) -> Self {
        Self::Out(OutBindSpec::new(sql_type))
    }

    /// Which way the bind carries data.
    #[must_use]
    pub const fn direction(&self) -> BindDirection {
        match self {
            Self::In(_) => BindDirection::In,
            Self::Out(_) => BindDirection::Out,
            Self::InOut { .. } => BindDirection::InOut,
        }
    }

    /// Whether the server writes a value back for this bind.
    #[must_use]
    pub const fn is_output(&self) -> bool {
        matches!(self, Self::Out(_) | Self::InOut { .. })
    }

    /// The value supplied to the server, if any.
    #[must_use]
    pub const fn value(&self) -> Option<&BindValue> {
        match self {
            Self::In(value) | Self::InOut { value, .. } => Some(value),
            Self::Out(_) => None,
        }
    }

    /// What is expected back, if this bind has an output direction.
    #[must_use]
    pub const fn out_spec(&self) -> Option<OutBindSpec> {
        match self {
            Self::Out(spec) | Self::InOut { spec, .. } => Some(*spec),
            Self::In(_) => None,
        }
    }
}

/// A bind addressed by name (`:employee_id`).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedBind {
    name: Box<str>,
    bind: Bind,
}

impl NamedBind {
    /// Names a bind. The name is written without its placeholder prefix.
    #[must_use]
    pub fn new(name: impl Into<Box<str>>, bind: Bind) -> Self {
        Self {
            name: name.into(),
            bind,
        }
    }

    /// The bind name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The bind itself.
    #[must_use]
    pub const fn bind(&self) -> &Bind {
        &self.bind
    }
}

/// How a statement's binds are addressed.
///
/// A statement uses one scheme or the other, never both: mixing them is a
/// vendor-specific behaviour the contract does not promise.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Binds {
    /// No binds.
    None,
    /// Binds addressed by position, in order.
    Positional(Vec<Bind>),
    /// Binds addressed by name.
    Named(Vec<NamedBind>),
}

impl Binds {
    /// How many binds there are.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Positional(binds) => binds.len(),
            Self::Named(binds) => binds.len(),
        }
    }

    /// Whether there are no binds.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether any bind expects a value back.
    #[must_use]
    pub fn has_outputs(&self) -> bool {
        match self {
            Self::None => false,
            Self::Positional(binds) => binds.iter().any(Bind::is_output),
            Self::Named(binds) => binds.iter().any(|named| named.bind().is_output()),
        }
    }
}

/// Statement text, its binds, and the options a driver needs before it executes.
///
/// ```
/// use reldex_db_driver_api::{Bind, OutBindSpec, SqlType, Statement};
///
/// let statement = Statement::new("BEGIN :out := add_one(:in); END;").with_named_binds(vec![
///     reldex_db_driver_api::NamedBind::new("out", Bind::output(SqlType::Number)),
///     reldex_db_driver_api::NamedBind::new("in", Bind::input(41_i64)),
/// ]);
///
/// assert_eq!(statement.binds().len(), 2);
/// assert!(statement.binds().has_outputs());
/// // Binds are plain data, so the same call can be executed again.
/// let again = statement.clone();
/// assert_eq!(again.binds().len(), 2);
/// let _ = OutBindSpec::new(SqlType::Number).with_max_size_bytes(64);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    sql: String,
    binds: Binds,
    deadline: Option<Duration>,
    fetch_rows: Option<NonZeroUsize>,
}

impl Statement {
    /// A statement with no binds and no options.
    #[must_use]
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            binds: Binds::None,
            deadline: None,
            fetch_rows: None,
        }
    }

    /// Attaches positional binds, replacing any previously attached binds.
    #[must_use]
    pub fn with_positional_binds(mut self, binds: Vec<Bind>) -> Self {
        self.binds = Binds::Positional(binds);
        self
    }

    /// Attaches named binds, replacing any previously attached binds.
    #[must_use]
    pub fn with_named_binds(mut self, binds: Vec<NamedBind>) -> Self {
        self.binds = Binds::Named(binds);
        self
    }

    /// Arms a deadline for this call, before it starts.
    ///
    /// The driver applies it to the whole round trip. It is set here, and not
    /// through the cancel handle, because on a driver whose cancellation is
    /// [`CancelKind::PreArmedDeadline`](crate::CancelKind::PreArmedDeadline)
    /// this is the *only* moment a limit can be established: once the call is
    /// running, that driver holds its own lock for the duration and cannot be
    /// interrupted. Arming a deadline up front is what makes such a statement
    /// stoppable at all (ADR-0002 D2, amendment M5).
    ///
    /// It is an upper bound, not a promise of precision: the driver stops at the
    /// next point its protocol allows. A driver that cannot apply a deadline at
    /// all must say so through
    /// [`Capabilities::cancel`](crate::Capabilities::cancel) and must not
    /// pretend the option took effect.
    ///
    /// When the deadline fires, the resulting error is
    /// [`crate::ErrorKind::Timeout`] — it was a limit the caller set, not a
    /// cancellation someone requested. A deadline a driver armed *in response
    /// to* a cancel request reports [`crate::ErrorKind::Cancelled`] instead.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Hints how many rows the driver should fetch per round trip.
    ///
    /// Drivers size their array fetch (and any read-ahead) when the statement is
    /// executed, not when rows are first asked for, so this cannot be deferred
    /// to [`Cursor::fetch_batch`](crate::Cursor::fetch_batch) — by then the
    /// first round trip has already happened with whatever default the driver
    /// chose. `fetch_batch` still bounds each individual batch; this bounds what
    /// the wire does underneath.
    ///
    /// A hint, not a requirement: a driver may clamp it, and must still return
    /// correct results if it ignores it entirely. [`crate::DEFAULT_FETCH_ROWS`]
    /// is a reasonable starting point until `phase-0.md` "Measurements" says
    /// otherwise.
    #[must_use]
    pub const fn with_fetch_rows(mut self, fetch_rows: NonZeroUsize) -> Self {
        self.fetch_rows = Some(fetch_rows);
        self
    }

    /// The statement text, exactly as submitted.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The binds.
    #[must_use]
    pub const fn binds(&self) -> &Binds {
        &self.binds
    }

    /// The deadline armed for this call, if any. See
    /// [`Statement::with_deadline`].
    #[must_use]
    pub const fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    /// The per-round-trip fetch hint, if any. See
    /// [`Statement::with_fetch_rows`].
    #[must_use]
    pub const fn fetch_rows(&self) -> Option<NonZeroUsize> {
        self.fetch_rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::BindValue;

    #[test]
    fn a_plain_statement_has_no_binds_and_no_options() {
        let statement = Statement::new("SELECT 1 FROM DUAL");
        assert_eq!(statement.sql(), "SELECT 1 FROM DUAL");
        assert!(statement.binds().is_empty());
        assert!(!statement.binds().has_outputs());
        assert!(matches!(statement.binds(), Binds::None));
        assert_eq!(statement.deadline(), None);
        assert_eq!(statement.fetch_rows(), None);
    }

    #[test]
    fn per_call_options_are_set_before_execute() {
        // A driver whose cancellation is a pre-armed deadline can only be
        // limited here; and every driver needs its fetch-array size before the
        // first round trip, not when rows are first requested.
        let statement = Statement::new("SELECT * FROM big_table")
            .with_deadline(Duration::from_secs(30))
            .with_fetch_rows(NonZeroUsize::new(500).expect("non-zero"));

        assert_eq!(statement.deadline(), Some(Duration::from_secs(30)));
        assert_eq!(statement.fetch_rows(), NonZeroUsize::new(500));
    }

    #[test]
    fn a_bound_statement_can_be_executed_again() {
        // Binds used to hold `Value`, which is not `Clone` because it may own a
        // live LOB or cursor; that made every bound statement single-use.
        let statement = Statement::new("SELECT * FROM t WHERE name = :1 AND note = :2")
            .with_positional_binds(vec![Bind::input("ข้อมูล"), Bind::input(None::<i64>)])
            .with_fetch_rows(NonZeroUsize::new(64).expect("non-zero"));

        let reused = statement.clone();
        assert_eq!(reused, statement);
        assert_eq!(reused.binds().len(), 2);
        assert_eq!(reused.fetch_rows(), NonZeroUsize::new(64));
        assert_eq!(
            reused.binds().len(),
            statement.binds().len(),
            "the original is still usable"
        );

        let Binds::Positional(binds) = reused.binds() else {
            panic!("expected positional binds");
        };
        assert_eq!(binds[0].value(), Some(&BindValue::from("ข้อมูล")));
        assert_eq!(binds[1].value(), Some(&BindValue::Null));
    }

    #[test]
    fn positional_binds_keep_their_order_and_direction() {
        let statement = Statement::new("UPDATE t SET a = :1 WHERE id = :2")
            .with_positional_binds(vec![Bind::input("new"), Bind::input(7_i64)]);
        let Binds::Positional(binds) = statement.binds() else {
            panic!("expected positional binds");
        };
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].direction(), BindDirection::In);
        assert!(binds[0].value().is_some());
        assert!(binds[0].out_spec().is_none());
        assert!(!statement.binds().has_outputs());
    }

    #[test]
    fn output_binds_declare_their_type_and_size() {
        let spec = OutBindSpec::new(SqlType::VARCHAR).with_max_size_bytes(4000);
        let statement = Statement::new("BEGIN p(:v); END;").with_named_binds(vec![NamedBind::new(
            "v",
            Bind::InOut {
                value: BindValue::from("seed"),
                spec,
            },
        )]);

        assert!(statement.binds().has_outputs());
        let Binds::Named(binds) = statement.binds() else {
            panic!("expected named binds");
        };
        assert_eq!(binds[0].name(), "v");
        assert_eq!(binds[0].bind().direction(), BindDirection::InOut);
        assert!(binds[0].bind().is_output());
        assert_eq!(
            binds[0]
                .bind()
                .out_spec()
                .and_then(OutBindSpec::max_size_bytes),
            Some(4000)
        );
        assert_eq!(
            binds[0].bind().out_spec().map(OutBindSpec::sql_type),
            Some(SqlType::VARCHAR)
        );
    }

    #[test]
    fn pure_output_binds_carry_no_value() {
        let bind = Bind::output(SqlType::Cursor);
        assert_eq!(bind.direction(), BindDirection::Out);
        assert!(bind.value().is_none());
        assert!(bind.is_output());
    }

    #[test]
    fn attaching_binds_replaces_the_previous_scheme() {
        let statement = Statement::new("SELECT :a FROM DUAL")
            .with_positional_binds(vec![Bind::input(1_i64)])
            .with_named_binds(vec![NamedBind::new("a", Bind::input(2_i64))]);
        assert!(matches!(statement.binds(), Binds::Named(_)));
        assert_eq!(statement.binds().len(), 1);
    }
}
