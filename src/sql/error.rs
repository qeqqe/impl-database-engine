use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    EmptyQuery,
    MultipleStatements,
    MissingClause(String),
    UnsupportedStatement(String),
    UnsupportedDataType(String),
    UnsupportedExpression(String),
    InvalidIdentifier(String),
    SqlParser(String),
}

pub type ParseResult<T> = Result<T, ParseError>;

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::EmptyQuery => write!(f, "Empty query"),
            ParseError::MultipleStatements => write!(f, "Multiple statements not allowed"),
            ParseError::MissingClause(c) => write!(f, "Missing clause: {}", c),
            ParseError::UnsupportedStatement(s) => write!(f, "Unsupported statement: {}", s),
            ParseError::UnsupportedDataType(dt) => write!(f, "Unsupported data type: {}", dt),
            ParseError::UnsupportedExpression(e) => write!(f, "Unsupported expression: {}", e),
            ParseError::InvalidIdentifier(id) => write!(f, "Invalid identifier: {}", id),
            ParseError::SqlParser(e) => write!(f, "SQL parser error: {}", e),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<sqlparser::parser::ParserError> for ParseError {
    fn from(err: sqlparser::parser::ParserError) -> Self {
        use sqlparser::parser::ParserError as E;
        ParseError::SqlParser(match err {
            E::ParserError(message) | E::TokenizerError(message) => message,
            E::RecursionLimitExceeded => "statement is nested too deeply".to_string(),
        })
    }
}
