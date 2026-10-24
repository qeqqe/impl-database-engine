use core::fmt;
use std::collections::HashSet;

use crate::{
    planner::{
        error::{PlanError, PlanResult},
        expr::{ScalarExpr, ScalarFunction},
        schema::Schema,
        types::DataType,
        value::{EvalError, EvalResult, Value, and3, bool_value, like_match, or3},
    },
    sql::{BinaryOperator, UnaryOperator},
};

#[derive(Debug, Clone, PartialEq)]
pub enum PhysExpr {
    Column {
        index: usize,
        name: String,
    },
    Literal(Value),
    Unary {
        op: UnaryOperator,
        expr: Box<PhysExpr>,
    },
    Binary {
        op: BinaryOperator,
        left: Box<PhysExpr>,
        right: Box<PhysExpr>,
    },
    IsNull {
        expr: Box<PhysExpr>,
        negated: bool,
    },
    InList {
        expr: Box<PhysExpr>,
        list: Vec<PhysExpr>,
        negated: bool,
    },
    InSet {
        expr: Box<PhysExpr>,
        set: HashSet<Value>,
        has_null: bool,
        negated: bool,
    },
    Like {
        expr: Box<PhysExpr>,
        pattern: String,
        negated: bool,
        case_insensitive: bool,
    },
    Function {
        func: ScalarFunction,
        args: Vec<PhysExpr>,
    },
    Cast {
        expr: Box<PhysExpr>,
        to: DataType,
    },
    Case {
        branches: Vec<(PhysExpr, PhysExpr)>,
        otherwise: Box<PhysExpr>,
    },
}

impl PhysExpr {
    pub fn lower(expr: &ScalarExpr, schema: &Schema) -> PlanResult<PhysExpr> {
        let lower = |e: &ScalarExpr| PhysExpr::lower(e, schema).map(Box::new);
        let lower_all = |items: &[ScalarExpr]| {
            items
                .iter()
                .map(|e| PhysExpr::lower(e, schema))
                .collect::<PlanResult<Vec<_>>>()
        };
        Ok(match expr {
            ScalarExpr::Column(c) => PhysExpr::Column {
                index: schema.index_of(c.id).ok_or_else(|| {
                    PlanError::Internal(format!(
                        "column {} ({}) not in input {schema}",
                        c.name, c.id
                    ))
                })?,
                name: c.name.clone(),
            },
            ScalarExpr::Literal(v) => PhysExpr::Literal(v.clone()),
            ScalarExpr::Unary { op, expr } => PhysExpr::Unary {
                op: *op,
                expr: lower(expr)?,
            },
            ScalarExpr::Binary { op, left, right } => PhysExpr::Binary {
                op: *op,
                left: lower(left)?,
                right: lower(right)?,
            },
            ScalarExpr::IsNull { expr, negated } => PhysExpr::IsNull {
                expr: lower(expr)?,
                negated: *negated,
            },
            ScalarExpr::InList {
                expr,
                list,
                negated,
            } => match literal_set(expr.data_type(), list) {
                Some((set, has_null)) => PhysExpr::InSet {
                    expr: lower(expr)?,
                    set,
                    has_null,
                    negated: *negated,
                },
                None => PhysExpr::InList {
                    expr: lower(expr)?,
                    list: lower_all(list)?,
                    negated: *negated,
                },
            },
            ScalarExpr::Subquery { .. }
            | ScalarExpr::Exists { .. }
            | ScalarExpr::InSubquery { .. } => {
                return Err(PlanError::Internal(format!(
                    "unresolved subquery {expr} reached the physical planner"
                )));
            }
            ScalarExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => PhysExpr::Like {
                expr: lower(expr)?,
                pattern: pattern.clone(),
                negated: *negated,
                case_insensitive: *case_insensitive,
            },
            ScalarExpr::Function { func, args } => PhysExpr::Function {
                func: *func,
                args: lower_all(args)?,
            },
            ScalarExpr::Cast { expr, to } => PhysExpr::Cast {
                expr: lower(expr)?,
                to: *to,
            },
            ScalarExpr::Case {
                branches,
                otherwise,
            } => PhysExpr::Case {
                branches: branches
                    .iter()
                    .map(|(condition, result)| {
                        Ok((
                            PhysExpr::lower(condition, schema)?,
                            PhysExpr::lower(result, schema)?,
                        ))
                    })
                    .collect::<PlanResult<Vec<_>>>()?,
                otherwise: lower(otherwise)?,
            },
        })
    }

    pub fn eval(&self, row: &[Value]) -> EvalResult<Value> {
        match self {
            PhysExpr::Column { index, .. } => {
                row.get(*index).cloned().ok_or(EvalError::ColumnOutOfRange {
                    index: *index,
                    width: row.len(),
                })
            }
            PhysExpr::Literal(v) => Ok(v.clone()),
            PhysExpr::Unary { op, expr } => Value::unary(*op, &expr.eval(row)?),
            PhysExpr::Binary {
                op: BinaryOperator::And,
                left,
                right,
            } => {
                let l = left.eval(row)?.to_bool()?;
                if l == Some(false) {
                    return Ok(Value::Boolean(false));
                }
                Ok(bool_value(and3(l, right.eval(row)?.to_bool()?)))
            }
            PhysExpr::Binary {
                op: BinaryOperator::Or,
                left,
                right,
            } => {
                let l = left.eval(row)?.to_bool()?;
                if l == Some(true) {
                    return Ok(Value::Boolean(true));
                }
                Ok(bool_value(or3(l, right.eval(row)?.to_bool()?)))
            }
            PhysExpr::Binary { op, left, right } => {
                Value::binary(*op, &left.eval(row)?, &right.eval(row)?)
            }
            PhysExpr::IsNull { expr, negated } => {
                Ok(Value::Boolean(expr.eval(row)?.is_null() != *negated))
            }
            PhysExpr::InList {
                expr,
                list,
                negated,
            } => {
                if list.is_empty() {
                    return Ok(Value::Boolean(*negated));
                }
                let needle = expr.eval(row)?;
                if needle.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for item in list {
                    match needle.sql_cmp(&item.eval(row)?)? {
                        Some(std::cmp::Ordering::Equal) => return Ok(Value::Boolean(!negated)),
                        Some(_) => {}
                        None => saw_null = true,
                    }
                }
                Ok(if saw_null {
                    Value::Null
                } else {
                    Value::Boolean(*negated)
                })
            }
            PhysExpr::InSet {
                expr,
                set,
                has_null,
                negated,
            } => {
                let needle = expr.eval(row)?;
                if needle.is_null() {
                    return Ok(Value::Null);
                }
                let found = match set.iter().next() {
                    Some(sample) if sample.data_type() != needle.data_type() => {
                        let mut found = false;
                        for item in set {
                            if needle.sql_cmp(item)? == Some(std::cmp::Ordering::Equal) {
                                found = true;
                                break;
                            }
                        }
                        found
                    }
                    _ => set.contains(&needle),
                };
                Ok(if found {
                    Value::Boolean(!negated)
                } else if *has_null {
                    Value::Null
                } else {
                    Value::Boolean(*negated)
                })
            }
            PhysExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => match expr.eval(row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Boolean(
                    like_match(&s, pattern, *case_insensitive) != *negated,
                )),
                other => Err(EvalError::TypeMismatch(format!(
                    "LIKE requires TEXT, got {}",
                    other.data_type()
                ))),
            },
            PhysExpr::Function { func, args } => eval_function(*func, args, row),
            PhysExpr::Cast { expr, to } => expr.eval(row)?.cast(*to),
            PhysExpr::Case {
                branches,
                otherwise,
            } => {
                for (condition, result) in branches {
                    if condition.eval_predicate(row)? {
                        return result.eval(row);
                    }
                }
                otherwise.eval(row)
            }
        }
    }

    pub fn eval_predicate(&self, row: &[Value]) -> EvalResult<bool> {
        Ok(self.eval(row)?.to_bool()? == Some(true))
    }
}

fn eval_function(func: ScalarFunction, args: &[PhysExpr], row: &[Value]) -> EvalResult<Value> {
    if func == ScalarFunction::Coalesce {
        for arg in args {
            let v = arg.eval(row)?;
            if !v.is_null() {
                return Ok(v);
            }
        }
        return Ok(Value::Null);
    }
    if func == ScalarFunction::NullIf {
        let [a, b] = args else {
            return Err(EvalError::TypeMismatch(format!(
                "NULLIF expects two arguments, got {}",
                args.len()
            )));
        };
        let a = a.eval(row)?;
        return Ok(match a.sql_cmp(&b.eval(row)?)? {
            Some(std::cmp::Ordering::Equal) => Value::Null,
            _ => a,
        });
    }
    if matches!(func, ScalarFunction::Round | ScalarFunction::Substr) {
        let values = args
            .iter()
            .map(|a| a.eval(row))
            .collect::<EvalResult<Vec<_>>>()?;
        if values.iter().any(Value::is_null) {
            return Ok(Value::Null);
        }
        return match (func, values.as_slice()) {
            (ScalarFunction::Round, [x]) => round(x, 0),
            (ScalarFunction::Round, [x, Value::Integer(digits)]) => round(x, *digits),
            (ScalarFunction::Substr, [Value::Text(s), Value::Integer(start)]) => {
                Ok(substr(s, *start, None))
            }
            (
                ScalarFunction::Substr,
                [Value::Text(s), Value::Integer(start), Value::Integer(len)],
            ) => {
                if *len < 0 {
                    return Err(EvalError::InvalidArgument(format!(
                        "negative SUBSTR length {len}"
                    )));
                }
                Ok(substr(s, *start, Some(*len)))
            }
            _ => Err(EvalError::TypeMismatch(format!(
                "{} cannot be applied to ({})",
                func.name(),
                values
                    .iter()
                    .map(|v| v.data_type().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        };
    }

    let [arg] = args else {
        return Err(EvalError::TypeMismatch(format!(
            "{} expects one argument, got {}",
            func.name(),
            args.len()
        )));
    };
    match (func, arg.eval(row)?) {
        (_, Value::Null) => Ok(Value::Null),
        (ScalarFunction::Upper, Value::Text(s)) => Ok(Value::Text(s.to_uppercase())),
        (ScalarFunction::Lower, Value::Text(s)) => Ok(Value::Text(s.to_lowercase())),
        (ScalarFunction::Length, Value::Text(s)) => Ok(Value::Integer(s.chars().count() as i64)),
        (ScalarFunction::Abs, Value::Integer(i)) => i
            .checked_abs()
            .map(Value::Integer)
            .ok_or_else(|| EvalError::Overflow(format!("ABS({i})"))),
        (ScalarFunction::Abs, Value::Float(x)) => Ok(Value::Float(x.abs())),
        (func, v) => Err(EvalError::TypeMismatch(format!(
            "{} cannot be applied to {}",
            func.name(),
            v.data_type()
        ))),
    }
}

fn round(value: &Value, digits: i64) -> EvalResult<Value> {
    match value {
        Value::Float(x) => {
            let digits = digits.clamp(-400, 400) as i32;
            if digits > 0 {
                let scale = 10f64.powi(digits);
                let scaled = x * scale;
                if !scaled.is_finite() {
                    return Ok(Value::Float(*x));
                }
                Ok(Value::Float(scaled.round() / scale))
            } else {
                let scale = 10f64.powi(-digits);
                if !scale.is_finite() {
                    return Ok(Value::Float(0.0));
                }
                Ok(Value::Float((x / scale).round() * scale))
            }
        }
        Value::Integer(i) if digits >= 0 => Ok(Value::Integer(*i)),
        Value::Integer(i) => {
            let Some(scale) = u32::try_from(-digits)
                .ok()
                .and_then(|d| 10i64.checked_pow(d))
            else {
                return Ok(Value::Integer(0));
            };
            let (quotient, remainder) = (i / scale, i % scale);
            let quotient = if remainder.unsigned_abs() * 2 >= scale.unsigned_abs() {
                quotient + i.signum()
            } else {
                quotient
            };
            quotient
                .checked_mul(scale)
                .map(Value::Integer)
                .ok_or_else(|| EvalError::Overflow(format!("ROUND({i}, {digits})")))
        }
        other => Err(EvalError::TypeMismatch(format!(
            "ROUND cannot be applied to {}",
            other.data_type()
        ))),
    }
}

fn substr(s: &str, start: i64, len: Option<i64>) -> Value {
    let end = len.map_or(i64::MAX, |len| start.saturating_add(len));
    let first = start.max(1);
    if end <= first {
        return Value::Text(String::new());
    }
    let skip = usize::try_from(first - 1).unwrap_or(usize::MAX);
    let take = usize::try_from(end - first).unwrap_or(usize::MAX);
    Value::Text(s.chars().skip(skip).take(take).collect())
}

pub const IN_SET_THRESHOLD: usize = 16;

fn literal_set(needle: DataType, list: &[ScalarExpr]) -> Option<(HashSet<Value>, bool)> {
    if list.len() < IN_SET_THRESHOLD {
        return None;
    }
    let mut set = HashSet::with_capacity(list.len());
    let mut has_null = false;
    for item in list {
        match item {
            ScalarExpr::Literal(Value::Null) => has_null = true,
            ScalarExpr::Literal(value) if value.data_type() == needle => {
                set.insert(value.clone());
            }
            _ => return None,
        }
    }
    Some((set, has_null))
}

impl fmt::Display for PhysExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhysExpr::Column { index, name } => write!(f, "{name}@{index}"),
            PhysExpr::Literal(Value::Text(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            PhysExpr::Literal(v) => write!(f, "{v}"),
            PhysExpr::Unary { op, expr } => write!(f, "{}({expr})", op.symbol()),
            PhysExpr::Binary { op, left, right } => write!(f, "({left} {} {right})", op.symbol()),
            PhysExpr::IsNull { expr, negated } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{expr} IS {not}NULL")
            }
            PhysExpr::InList {
                expr,
                list,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "{expr} {not}IN (")?;
                super::logical::write_joined(f, list)?;
                f.write_str(")")
            }
            PhysExpr::InSet {
                expr,
                set,
                has_null,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                let count = set.len() + usize::from(*has_null);
                write!(f, "{expr} {not}IN ({count} values)")
            }
            PhysExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let not = if *negated { "NOT " } else { "" };
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(f, "{expr} {not}{kw} '{}'", pattern.replace('\'', "''"))
            }
            PhysExpr::Function { func, args } => {
                write!(f, "{}(", func.name())?;
                super::logical::write_joined(f, args)?;
                f.write_str(")")
            }
            PhysExpr::Cast { expr, to } => write!(f, "CAST({expr} AS {to})"),
            PhysExpr::Case {
                branches,
                otherwise,
            } => {
                f.write_str("CASE")?;
                for (condition, result) in branches {
                    write!(f, " WHEN {condition} THEN {result}")?;
                }
                write!(f, " ELSE {otherwise} END")
            }
        }
    }
}
