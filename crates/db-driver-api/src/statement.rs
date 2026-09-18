//! Statement text and bind parameters (ADR-0002 D6).
//!
//! A [`Statement`] is literal SQL or PL/SQL text plus its binds. The contract
//! does not parse it, split it, or decide what kind of statement it is: that is
//! the server's job, and a worksheet must be able to submit whatever the user
//! typed (`SPEC.md` §15 keeps script splitting out of this layer).

use crate::types::SqlType;
use crate::value::Value;

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
#[derive(Debug)]
pub enum Bind {
    /// A value supplied to the server.
    In(Value),
    /// A placeholder the server fills in.
    Out(OutBindSpec),
    /// A value supplied to the server and overwritten by it.
    InOut {
        /// The value supplied.
        value: Value,
        /// What is expected back.
        spec: OutBindSpec,
    },
}

impl Bind {
    /// An IN bind.
    pub fn input(value: impl Into<Value>) -> Self {
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
    pub const fn value(&self) -> Option<&Value> {
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
#[derive(Debug)]
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
#[derive(Debug)]
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

/// Statement text plus its binds.
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
/// let _ = OutBindSpec::new(SqlType::Number).with_max_size_bytes(64);
/// ```
#[derive(Debug)]
pub struct Statement {
    sql: String,
    binds: Binds,
}

impl Statement {
    /// A statement with no binds.
    #[must_use]
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            binds: Binds::None,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_statement_has_no_binds() {
        let statement = Statement::new("SELECT 1 FROM DUAL");
        assert_eq!(statement.sql(), "SELECT 1 FROM DUAL");
        assert!(statement.binds().is_empty());
        assert!(!statement.binds().has_outputs());
        assert!(matches!(statement.binds(), Binds::None));
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
                value: Value::from("seed"),
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
