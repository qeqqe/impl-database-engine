use crate::{
    planner::{
        error::{PlanError, PlanResult},
        schema::ColumnId,
        types::DataType,
        value::Value,
    },
    sql::{BinaryOperator, UnaryOperator},
};

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRef {
    pub id: ColumnId,
    pub name: String,
    pub data_type: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScalarExpr {
    Column(ColumnRef),
    Literal(Value),
    Unary {
        op: UnaryOperator,
        expr: Box<ScalarExpr>,
    },
    Binary {
        op: BinaryOperator,
        left: Box<ScalarExpr>,
        right: Box<ScalarExpr>,
    },
    IsNull {
        expr: Box<ScalarExpr>,
        negated: bool,
    },
    InList {
        expr: Box<ScalarExpr>,
        list: Vec<ScalarExpr>,
        negated: bool,
    },
    Like {
        expr: Box<ScalarExpr>,
        pattern: String,
        negated: bool,
        case_insensitive: bool,
    },
    Function {
        func: ScalarFunction,
        args: Vec<ScalarExpr>,
    },
    Cast {
        expr: Box<ScalarExpr>,
        to: DataType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarFunction {
    Upper,
    Lower,
    Length,
    Abs,
    Coalesce,
}

impl ScalarFunction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "UPPER" => Some(ScalarFunction::Upper),
            "LOWER" => Some(ScalarFunction::Lower),
            "LENGTH" | "CHAR_LENGTH" => Some(ScalarFunction::Length),
            "ABS" => Some(ScalarFunction::Abs),
            "COALESCE" => Some(ScalarFunction::Coalesce),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ScalarFunction::Upper => "UPPER",
            ScalarFunction::Lower => "LOWER",
            ScalarFunction::Length => "LENGTH",
            ScalarFunction::Abs => "ABS",
            ScalarFunction::Coalesce => "COALESCE",
        }
    }

    pub fn return_type(&self, args: &[DataType]) -> PlanResult<DataType> {
        let mismatch = |expected: &str| {
            Err(PlanError::TypeMismatch(format!(
                "{} expects {}, got ({})",
                self.name(),
                expected,
                args.iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        };
        match self {
            ScalarFunction::Upper | ScalarFunction::Lower => match args {
                [t] if t.is_textual() || t.is_null() => Ok(DataType::Text),
                _ => mismatch("one TEXT argument"),
            },
            ScalarFunction::Length => match args {
                [t] if t.is_textual() || t.is_null() => Ok(DataType::Integer),
                _ => mismatch("one TEXT argument"),
            },
            ScalarFunction::Abs => match args {
                [t] if t.is_numeric() || t.is_null() => Ok(*t),
                _ => mismatch("one numeric argument"),
            },
            ScalarFunction::Coalesce => {
                if args.is_empty() {
                    return mismatch("at least one argument");
                }
                args.iter()
                    .try_fold(DataType::Null, |acc, t| acc.unify(*t))
                    .map_or_else(|| mismatch("arguments of a common type"), Ok)
            }
        }
    }
}

pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Max,
    Min,
}

impl AggregateFunction {
    pub fn name(&self) -> &'static str {
        match self {
            AggregateFunction::Count => "COUNT",
            AggregateFunction::Sum => "SUM",
            AggregateFunction::Avg => "AVG",
            AggregateFunction::Max => "MAX",
            AggregateFunction::Min => "MIN",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggregateFunction::Count),
            "SUM" => Some(AggregateFunction::Sum),
            "AVG" => Some(AggregateFunction::Avg),
            "MAX" => Some(AggregateFunction::Max),
            "MIN" => Some(AggregateFunction::Min),
            _ => None,
        }
    }

    pub fn return_type(&self, arg: Option<DataType>) -> PlanResult<DataType> {
        let arg_type = match (self, arg) {
            (AggregateFunction::Count, _) => return Ok(DataType::Integer),
            (_, None) => {
                return Err(PlanError::InvalidAggregate(format!(
                    "{}(*) is only valid for COUNT",
                    self.name()
                )));
            }
            (_, Some(t)) => t,
        };

        match self {
            AggregateFunction::Sum if arg_type.is_numeric() => Ok(arg_type),
            AggregateFunction::Sum if arg_type.is_null() => Ok(DataType::Integer),
            AggregateFunction::Avg if arg_type.is_numeric() || arg_type.is_null() => {
                Ok(DataType::Float)
            }
            AggregateFunction::Min | AggregateFunction::Max if arg_type != DataType::Boolean => {
                Ok(arg_type)
            }

            _ => Err(PlanError::TypeMismatch(format!(
                "{} cannot be applied to {}",
                self.name(),
                arg_type
            ))),
        }
    }
}
