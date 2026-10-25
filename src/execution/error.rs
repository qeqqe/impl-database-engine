use std::fmt;

use crate::planner::value::EvalError;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecError {
    Eval(EvalError),
    TableNotFound(String),
    Storage(String),
    Protocol(String),
}

pub type ExecResult<T> = Result<T, ExecError>;

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Eval(e) => write!(f, "{e}"),
            ExecError::TableNotFound(t) => write!(f, "Table not found in storage: {t}"),
            ExecError::Storage(msg) => write!(f, "Storage error: {msg}"),
            ExecError::Protocol(msg) => write!(f, "Operator protocol violation: {msg}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<EvalError> for ExecError {
    fn from(err: EvalError) -> Self {
        ExecError::Eval(err)
    }
}

impl From<Box<dyn std::error::Error>> for ExecError {
    fn from(err: Box<dyn std::error::Error>) -> Self {
        ExecError::Storage(err.to_string())
    }
}
