use std::fmt;

use crate::{
    execution::error::ExecError,
    planner::{error::PlanError, value::EvalError},
    sql::ParseError,
};

#[derive(Debug, Clone, PartialEq)]
pub enum DbError {
    Parse(ParseError),
    Plan(PlanError),
    Exec(ExecError),
    NotNullViolation {
        table: String,
        column: String,
    },
    DuplicateKey {
        table: String,
        key: String,
    },
    UniqueViolation {
        table: String,
        columns: Vec<String>,
    },
    CheckViolation {
        table: String,
        constraint: String,
    },
    KeyOutOfRange {
        table: String,
        key: i64,
    },
    RowTooLarge {
        table: String,
        bytes: usize,
        limit: usize,
    },
    TooManyTables,
    Locked(String),
    IncompatibleFormat {
        found: String,
        expected: &'static str,
    },
    Transaction(String),
    Busy(String),
    SessionsOpen(usize),
    RolledBack(Box<DbError>),
    Storage(String),
    Corrupt(String),
    Internal(String),
}

pub type DbResult<T> = Result<T, DbError>;

impl DbError {
    pub fn storage(err: impl fmt::Display) -> Self {
        DbError::Storage(err.to_string())
    }

    pub fn root_cause(&self) -> &DbError {
        match self {
            DbError::RolledBack(inner) => inner.root_cause(),
            other => other,
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Parse(e) => write!(f, "{e}"),
            DbError::Plan(e) => write!(f, "{e}"),
            DbError::Exec(e) => write!(f, "{e}"),
            DbError::NotNullViolation { table, column } => {
                write!(
                    f,
                    "NULL value violates NOT NULL constraint on {table}.{column}"
                )
            }
            DbError::DuplicateKey { table, key } => {
                write!(f, "duplicate primary key {key} in table {table}")
            }
            DbError::UniqueViolation { table, columns } => write!(
                f,
                "duplicate value violates UNIQUE ({}) on table {table}",
                columns.join(", ")
            ),
            DbError::CheckViolation { table, constraint } => {
                write!(f, "row of table {table} violates {constraint}")
            }
            DbError::KeyOutOfRange { table, key } => write!(
                f,
                "primary key {key} of table {table} is outside the supported range"
            ),
            DbError::RowTooLarge {
                table,
                bytes,
                limit,
            } => write!(
                f,
                "row of {bytes} bytes for table {table} exceeds the {limit} byte limit"
            ),
            DbError::TooManyTables => f.write_str("table id space exhausted"),
            DbError::Locked(dir) => {
                write!(f, "database {dir} is already open in another process")
            }
            DbError::IncompatibleFormat { found, expected } => write!(
                f,
                "incompatible database format: found {found}, expected {expected}"
            ),
            DbError::Transaction(msg) => write!(f, "transaction error: {msg}"),
            DbError::Busy(msg) => write!(f, "database is busy: {msg}"),
            DbError::SessionsOpen(n) => {
                write!(
                    f,
                    "cannot close the database while {n} other sessions are open"
                )
            }
            DbError::RolledBack(inner) => write!(f, "{inner} (transaction rolled back)"),
            DbError::Storage(msg) => write!(f, "storage error: {msg}"),
            DbError::Corrupt(msg) => write!(f, "corrupt data: {msg}"),
            DbError::Internal(msg) => write!(f, "internal executor error: {msg}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<ParseError> for DbError {
    fn from(err: ParseError) -> Self {
        DbError::Parse(err)
    }
}

impl From<PlanError> for DbError {
    fn from(err: PlanError) -> Self {
        match err {
            PlanError::Parse(e) => DbError::Parse(e),
            other => DbError::Plan(other),
        }
    }
}

impl From<ExecError> for DbError {
    fn from(err: ExecError) -> Self {
        DbError::Exec(err)
    }
}

impl From<EvalError> for DbError {
    fn from(err: EvalError) -> Self {
        DbError::Exec(ExecError::Eval(err))
    }
}
