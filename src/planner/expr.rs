use std::collections::BTreeSet;
use std::fmt;

use crate::planner::logical::LogicalPlan;
use crate::sql::{BinaryOperator, UnaryOperator};

use super::error::{PlanError, PlanResult};
use super::schema::{ColumnId, Schema};
use super::types::DataType;
use super::value::Value;

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
    Case {
        branches: Vec<(ScalarExpr, ScalarExpr)>,
        otherwise: Box<ScalarExpr>,
    },
    Subquery {
        plan: Box<LogicalPlan>,
    },
    Exists {
        plan: Box<LogicalPlan>,
        negated: bool,
    },
    InSubquery {
        expr: Box<ScalarExpr>,
        plan: Box<LogicalPlan>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarFunction {
    Upper,
    Lower,
    Length,
    Abs,
    Coalesce,
    NullIf,
    Round,
    Substr,
}

impl ScalarFunction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "UPPER" => Some(ScalarFunction::Upper),
            "LOWER" => Some(ScalarFunction::Lower),
            "LENGTH" | "CHAR_LENGTH" => Some(ScalarFunction::Length),
            "ABS" => Some(ScalarFunction::Abs),
            "COALESCE" => Some(ScalarFunction::Coalesce),
            "NULLIF" => Some(ScalarFunction::NullIf),
            "ROUND" => Some(ScalarFunction::Round),
            "SUBSTR" | "SUBSTRING" => Some(ScalarFunction::Substr),
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
            ScalarFunction::NullIf => "NULLIF",
            ScalarFunction::Round => "ROUND",
            ScalarFunction::Substr => "SUBSTR",
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
            ScalarFunction::NullIf => match args {
                [a, b] if a.is_comparable_with(*b) => Ok(if a.is_null() { *b } else { *a }),
                _ => mismatch("two comparable arguments"),
            },
            ScalarFunction::Round => match args {
                [t] if t.is_numeric() || t.is_null() => Ok(*t),
                [t, digits]
                    if (t.is_numeric() || t.is_null())
                        && matches!(digits, DataType::Integer | DataType::Null) =>
                {
                    Ok(*t)
                }
                _ => mismatch("a numeric argument and an optional INTEGER digit count"),
            },
            ScalarFunction::Substr => match args {
                [s, rest @ ..]
                    if (s.is_textual() || s.is_null())
                        && matches!(rest.len(), 1 | 2)
                        && rest
                            .iter()
                            .all(|t| matches!(t, DataType::Integer | DataType::Null)) =>
                {
                    Ok(DataType::Text)
                }
                _ => mismatch("a TEXT argument, an INTEGER start and an optional INTEGER length"),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Max,
    Min,
}

impl AggregateFunction {
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

    pub fn name(&self) -> &'static str {
        match self {
            AggregateFunction::Count => "COUNT",
            AggregateFunction::Sum => "SUM",
            AggregateFunction::Avg => "AVG",
            AggregateFunction::Min => "MIN",
            AggregateFunction::Max => "MAX",
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

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    pub func: AggregateFunction,
    pub arg: Option<ScalarExpr>,
    pub distinct: bool,
}

impl AggregateCall {
    pub fn data_type(&self) -> DataType {
        self.func
            .return_type(self.arg.as_ref().map(ScalarExpr::data_type))
            .unwrap_or(DataType::Null)
    }

    pub fn nullable(&self) -> bool {
        self.func != AggregateFunction::Count
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: ScalarExpr,
    pub ascending: bool,
    pub nulls_first: bool,
}

impl ScalarExpr {
    pub fn column(id: ColumnId, name: impl Into<String>, data_type: DataType) -> ScalarExpr {
        ScalarExpr::Column(ColumnRef {
            id,
            name: name.into(),
            data_type,
        })
    }

    pub fn literal(value: impl Into<Value>) -> ScalarExpr {
        ScalarExpr::Literal(value.into())
    }

    pub fn binary(op: BinaryOperator, left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
        ScalarExpr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    pub fn and(left: ScalarExpr, right: ScalarExpr) -> ScalarExpr {
        ScalarExpr::binary(BinaryOperator::And, left, right)
    }

    pub fn negate(expr: ScalarExpr) -> ScalarExpr {
        ScalarExpr::Unary {
            op: UnaryOperator::Not,
            expr: Box::new(expr),
        }
    }

    pub fn cast(self, to: DataType) -> ScalarExpr {
        if self.data_type() == to || matches!(self, ScalarExpr::Literal(Value::Null)) {
            self
        } else {
            ScalarExpr::Cast {
                expr: Box::new(self),
                to,
            }
        }
    }

    pub fn data_type(&self) -> DataType {
        match self {
            ScalarExpr::Column(c) => c.data_type,
            ScalarExpr::Literal(v) => v.data_type(),
            ScalarExpr::Unary {
                op: UnaryOperator::Not,
                ..
            } => DataType::Boolean,
            ScalarExpr::Unary { expr, .. } => expr.data_type(),
            ScalarExpr::Binary { op, left, right } => {
                if op.is_comparison() || op.is_logical() {
                    DataType::Boolean
                } else if *op == BinaryOperator::Concat {
                    DataType::Text
                } else {
                    left.data_type()
                        .numeric_result(right.data_type())
                        .unwrap_or(DataType::Null)
                }
            }
            ScalarExpr::IsNull { .. }
            | ScalarExpr::InList { .. }
            | ScalarExpr::Like { .. }
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. } => DataType::Boolean,
            ScalarExpr::Subquery { plan } => plan
                .schema()
                .fields()
                .first()
                .map_or(DataType::Null, |f| f.data_type),
            ScalarExpr::Function { func, args } => {
                let types: Vec<DataType> = args.iter().map(ScalarExpr::data_type).collect();
                func.return_type(&types).unwrap_or(DataType::Null)
            }
            ScalarExpr::Cast { to, .. } => *to,
            ScalarExpr::Case {
                branches,
                otherwise,
            } => branches
                .iter()
                .map(|(_, result)| result.data_type())
                .chain(std::iter::once(otherwise.data_type()))
                .try_fold(DataType::Null, DataType::unify)
                .unwrap_or(DataType::Null),
        }
    }

    pub fn nullable(&self, schema: &Schema) -> bool {
        match self {
            ScalarExpr::Column(c) => schema
                .index_of(c.id)
                .is_none_or(|i| schema.field(i).nullable),
            ScalarExpr::Literal(v) => v.is_null(),
            ScalarExpr::IsNull { .. } | ScalarExpr::Exists { .. } => false,
            ScalarExpr::Subquery { .. } => true,
            ScalarExpr::InSubquery { expr, plan, .. } => {
                expr.nullable(schema) || plan.schema().fields().iter().any(|f| f.nullable)
            }
            ScalarExpr::Function {
                func: ScalarFunction::Coalesce,
                args,
            } => args.iter().all(|a| a.nullable(schema)),
            ScalarExpr::Function {
                func: ScalarFunction::NullIf,
                ..
            } => true,
            ScalarExpr::Case {
                branches,
                otherwise,
            } => otherwise.nullable(schema) || branches.iter().any(|(_, r)| r.nullable(schema)),
            other => other.children().iter().any(|c| c.nullable(schema)),
        }
    }

    pub fn children(&self) -> Vec<&ScalarExpr> {
        match self {
            ScalarExpr::Column(_)
            | ScalarExpr::Literal(_)
            | ScalarExpr::Subquery { .. }
            | ScalarExpr::Exists { .. } => vec![],
            ScalarExpr::Unary { expr, .. }
            | ScalarExpr::IsNull { expr, .. }
            | ScalarExpr::Like { expr, .. }
            | ScalarExpr::Cast { expr, .. }
            | ScalarExpr::InSubquery { expr, .. } => vec![expr],
            ScalarExpr::Binary { left, right, .. } => vec![left, right],
            ScalarExpr::InList { expr, list, .. } => {
                std::iter::once(expr.as_ref()).chain(list.iter()).collect()
            }
            ScalarExpr::Function { args, .. } => args.iter().collect(),
            ScalarExpr::Case {
                branches,
                otherwise,
            } => branches
                .iter()
                .flat_map(|(condition, result)| [condition, result])
                .chain(std::iter::once(otherwise.as_ref()))
                .collect(),
        }
    }

    pub fn map_children<F>(self, f: F) -> PlanResult<ScalarExpr>
    where
        F: FnMut(ScalarExpr) -> PlanResult<ScalarExpr>,
    {
        self.try_map_children(f)
    }

    pub fn try_map_children<E, F>(self, mut f: F) -> Result<ScalarExpr, E>
    where
        F: FnMut(ScalarExpr) -> Result<ScalarExpr, E>,
    {
        let mut boxed = |e: Box<ScalarExpr>| -> Result<Box<ScalarExpr>, E> { Ok(Box::new(f(*e)?)) };
        Ok(match self {
            leaf @ (ScalarExpr::Column(_)
            | ScalarExpr::Literal(_)
            | ScalarExpr::Subquery { .. }
            | ScalarExpr::Exists { .. }) => leaf,
            ScalarExpr::InSubquery {
                expr,
                plan,
                negated,
            } => ScalarExpr::InSubquery {
                expr: boxed(expr)?,
                plan,
                negated,
            },
            ScalarExpr::Unary { op, expr } => ScalarExpr::Unary {
                op,
                expr: boxed(expr)?,
            },
            ScalarExpr::Binary { op, left, right } => ScalarExpr::Binary {
                op,
                left: boxed(left)?,
                right: boxed(right)?,
            },
            ScalarExpr::IsNull { expr, negated } => ScalarExpr::IsNull {
                expr: boxed(expr)?,
                negated,
            },
            ScalarExpr::InList {
                expr,
                list,
                negated,
            } => ScalarExpr::InList {
                expr: boxed(expr)?,
                list: list
                    .into_iter()
                    .map(|e| boxed(Box::new(e)).map(|b| *b))
                    .collect::<Result<Vec<_>, E>>()?,
                negated,
            },
            ScalarExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => ScalarExpr::Like {
                expr: boxed(expr)?,
                pattern,
                negated,
                case_insensitive,
            },
            ScalarExpr::Function { func, args } => ScalarExpr::Function {
                func,
                args: args
                    .into_iter()
                    .map(|e| boxed(Box::new(e)).map(|b| *b))
                    .collect::<Result<Vec<_>, E>>()?,
            },
            ScalarExpr::Cast { expr, to } => ScalarExpr::Cast {
                expr: boxed(expr)?,
                to,
            },
            ScalarExpr::Case {
                branches,
                otherwise,
            } => ScalarExpr::Case {
                branches: branches
                    .into_iter()
                    .map(|(condition, result)| {
                        Ok((*boxed(Box::new(condition))?, *boxed(Box::new(result))?))
                    })
                    .collect::<Result<Vec<_>, E>>()?,
                otherwise: boxed(otherwise)?,
            },
        })
    }

    pub fn transform_up<F>(self, f: &mut F) -> PlanResult<ScalarExpr>
    where
        F: FnMut(ScalarExpr) -> PlanResult<ScalarExpr>,
    {
        self.try_transform_up(f)
    }

    pub fn try_transform_up<E, F>(self, f: &mut F) -> Result<ScalarExpr, E>
    where
        F: FnMut(ScalarExpr) -> Result<ScalarExpr, E>,
    {
        let rewritten = self.try_map_children(|child| child.try_transform_up(f))?;
        f(rewritten)
    }

    pub fn transform_down<F>(self, f: &mut F) -> PlanResult<ScalarExpr>
    where
        F: FnMut(&ScalarExpr) -> PlanResult<Option<ScalarExpr>>,
    {
        match f(&self)? {
            Some(replacement) => Ok(replacement),
            None => self.map_children(|child| child.transform_down(f)),
        }
    }

    pub fn any(&self, predicate: &mut impl FnMut(&ScalarExpr) -> bool) -> bool {
        predicate(self) || self.children().into_iter().any(|c| c.any(predicate))
    }

    pub fn column_ids(&self) -> BTreeSet<ColumnId> {
        let mut out = BTreeSet::new();
        self.collect_column_ids(&mut out);
        out
    }

    pub fn collect_column_ids(&self, out: &mut BTreeSet<ColumnId>) {
        if let ScalarExpr::Column(c) = self {
            out.insert(c.id);
        }
        for child in self.children() {
            child.collect_column_ids(out);
        }
    }

    pub fn is_bound_by(&self, schema: &Schema) -> bool {
        self.column_ids().iter().all(|id| schema.contains(*id))
    }

    pub fn is_constant(&self) -> bool {
        !self.any(&mut |e| matches!(e, ScalarExpr::Column(_)) || e.is_subquery())
    }

    pub fn is_subquery(&self) -> bool {
        matches!(
            self,
            ScalarExpr::Subquery { .. } | ScalarExpr::Exists { .. } | ScalarExpr::InSubquery { .. }
        )
    }

    pub fn contains_subquery(&self) -> bool {
        self.any(&mut |e| e.is_subquery())
    }

    pub fn as_literal_bool(&self) -> Option<Option<bool>> {
        match self {
            ScalarExpr::Literal(Value::Boolean(b)) => Some(Some(*b)),
            ScalarExpr::Literal(Value::Null) => Some(None),
            _ => None,
        }
    }

    pub fn split_conjunction(self) -> Vec<ScalarExpr> {
        match self {
            ScalarExpr::Binary {
                op: BinaryOperator::And,
                left,
                right,
            } => {
                let mut out = left.split_conjunction();
                out.extend(right.split_conjunction());
                out
            }
            other => vec![other],
        }
    }

    pub fn conjunction(exprs: impl IntoIterator<Item = ScalarExpr>) -> Option<ScalarExpr> {
        exprs.into_iter().reduce(ScalarExpr::and)
    }
}

impl fmt::Display for ScalarExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarExpr::Column(c) => f.write_str(&c.name),
            ScalarExpr::Literal(Value::Text(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            ScalarExpr::Literal(v) => write!(f, "{v}"),
            ScalarExpr::Unary { op, expr } => write!(f, "{}{}", op.symbol(), Paren(expr)),
            ScalarExpr::Binary { op, left, right } => {
                write!(f, "{} {} {}", Paren(left), op.symbol(), Paren(right))
            }
            ScalarExpr::IsNull { expr, negated } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{} IS {not}NULL", Paren(expr))
            }
            ScalarExpr::InList {
                expr,
                list,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{} {not}IN (", Paren(expr))?;
                for (i, item) in list.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str(")")
            }
            ScalarExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let not = if *negated { "NOT " } else { "" };
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(
                    f,
                    "{} {not}{kw} '{}'",
                    Paren(expr),
                    pattern.replace('\'', "''")
                )
            }
            ScalarExpr::Function { func, args } => {
                write!(f, "{}(", func.name())?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                f.write_str(")")
            }
            ScalarExpr::Cast { expr, to } => write!(f, "CAST({expr} AS {to})"),
            ScalarExpr::Case {
                branches,
                otherwise,
            } => {
                f.write_str("CASE")?;
                for (condition, result) in branches {
                    write!(f, " WHEN {condition} THEN {result}")?;
                }
                write!(f, " ELSE {otherwise} END")
            }
            ScalarExpr::Subquery { .. } => f.write_str("(SUBQUERY)"),
            ScalarExpr::Exists { negated, .. } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{not}EXISTS (SUBQUERY)")
            }
            ScalarExpr::InSubquery { expr, negated, .. } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{} {not}IN (SUBQUERY)", Paren(expr))
            }
        }
    }
}

struct Paren<'a>(&'a ScalarExpr);

impl fmt::Display for Paren<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            e @ (ScalarExpr::Binary { .. }
            | ScalarExpr::IsNull { .. }
            | ScalarExpr::InList { .. }
            | ScalarExpr::InSubquery { .. }
            | ScalarExpr::Like { .. }) => write!(f, "({e})"),
            e => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for AggregateCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let distinct = if self.distinct { "DISTINCT " } else { "" };
        match &self.arg {
            None => write!(f, "{}(*)", self.func.name()),
            Some(arg) => write!(f, "{}({distinct}{arg})", self.func.name()),
        }
    }
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = if self.ascending { "ASC" } else { "DESC" };
        let nulls = if self.nulls_first { "FIRST" } else { "LAST" };
        write!(f, "{} {dir} NULLS {nulls}", self.expr)
    }
}
