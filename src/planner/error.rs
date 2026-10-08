use core::fmt;

use crate::sql::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub enum PlanError {
    Parse(ParseError),
    TableNotFound(String),
    TableAlreadyExists(String),
    ColumnNotFound {
        table: Option<String>,
        column: String,
    },
    AmbiguousColumn {
        column: String,
        candidates: Vec<String>,
    },
    AliasNotFound(String),
    DuplicateAlias(String),
    DuplicateColumn(String),
    AggregateNotAllowed {
        clause: &'static str,
    },
    NestedAggregate(String),
    InvalidAggregate(String),
    NonAggregateColumn {
        column: String,
    },
    UnknownFunction(String),
    TypeMismatch(String),
    InvalidOrderBy(String),
    PositionOutOfRange {
        clause: &'static str,
        position: i64,
    },
    InvalidInsert(String),
    NotNullViolation {
        table: String,
        column: String,
    },
    Unsupported(String),
    Internal(String),
}

pub type PlanResult<T> = Result<T, PlanError>;

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Parse(e) => write!(f, "{e}"),
            PlanError::TableNotFound(name) => write!(f, "No table with named \"{name}\" found."),
            PlanError::TableAlreadyExists(name) => write!(f, "Table \"{name}\" already exists."),
            PlanError::ColumnNotFound {
                table: Some(t),
                column,
            } => write!(f, "Column not found: {t}.{column}"),
            PlanError::ColumnNotFound {
                table: None,
                column,
            } => write!(f, "Column not found: {column}"),
            PlanError::AmbiguousColumn { column, candidates } => write!(
                f,
                "Ambiguous column '{column}', found in tables: {}",
                candidates.join(", ")
            ),
            PlanError::AliasNotFound(a) => write!(f, "Table alias not found: {a}"),
            PlanError::DuplicateAlias(a) => write!(f, "Duplicate table alias: {a}"),
            PlanError::DuplicateColumn(c) => write!(f, "Column specified more than once: {c}"),
            PlanError::AggregateNotAllowed { clause } => {
                write!(f, "Aggregate functions are not allowed in {clause}")
            }
            PlanError::NestedAggregate(e) => {
                write!(f, "Aggregate function calls cannot be nested: {e}")
            }
            PlanError::InvalidAggregate(msg) => write!(f, "Invalid aggregate: {msg}"),
            PlanError::NonAggregateColumn { column } => write!(
                f,
                "Column '{column}' must appear in GROUP BY or be used in an aggregate function"
            ),
            PlanError::UnknownFunction(name) => write!(f, "Unknown function: {name}"),
            PlanError::TypeMismatch(msg) => write!(f, "Type mismatch: {msg}"),
            PlanError::InvalidOrderBy(msg) => write!(f, "Invalid ORDER BY: {msg}"),
            PlanError::PositionOutOfRange { clause, position } => {
                write!(f, "{clause} position {position} is not in select list")
            }
            PlanError::InvalidInsert(msg) => write!(f, "Invalid INSERT: {msg}"),
            PlanError::NotNullViolation { table, column } => {
                write!(
                    f,
                    "NULL value violates NOT NULL constraint on {table}.{column}"
                )
            }
            PlanError::Unsupported(_) => todo!(),
            PlanError::Internal(_) => todo!(),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<ParseError> for PlanError {
    fn from(err: ParseError) -> Self {
        PlanError::Parse(err)
    }
}
