use core::fmt;

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
}

impl PhysExpr {
    pub fn lower(expr: &ScalarExpr, schema: &Schema) -> PlanResult<PhysExpr> {
        let lower = |e: &ScalarExpr| PhysExpr::lower(e, schema).map(Box::new);
        let lower_all = |item: &[ScalarExpr]| {
            item.iter()
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
            } => PhysExpr::InList {
                expr: lower(expr)?,
                list: lower_all(list)?,
                negated: *negated,
            },
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
        }
    }
}
