use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::sql::{BinaryOperator, LiteralValue, UnaryOperator};

use super::types::DataType;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

pub type Row = Vec<Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    TypeMismatch(String),
    DivisionByZero,
    Overflow(String),
    InvalidCast { value: String, to: DataType },
    ColumnOutOfRange { index: usize, width: usize },
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
            EvalError::DivisionByZero => f.write_str("division by zero"),
            EvalError::Overflow(msg) => write!(f, "numeric overflow: {msg}"),
            EvalError::InvalidCast { value, to } => write!(f, "cannot cast {value} to {to}"),
            EvalError::ColumnOutOfRange { index, width } => {
                write!(
                    f,
                    "column position {index} out of range for row of width {width}"
                )
            }
        }
    }
}

impl std::error::Error for EvalError {}

pub type EvalResult<T> = Result<T, EvalError>;

impl Value {
    pub fn data_type(&self) -> DataType {
        match self {
            Value::Null => DataType::Null,
            Value::Boolean(_) => DataType::Boolean,
            Value::Integer(_) => DataType::Integer,
            Value::Float(_) => DataType::Float,
            Value::Text(_) => DataType::Text,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn to_bool(&self) -> EvalResult<Option<bool>> {
        match self {
            Value::Null => Ok(None),
            Value::Boolean(b) => Ok(Some(*b)),
            other => Err(EvalError::TypeMismatch(format!(
                "expected BOOLEAN, got {}",
                other.data_type()
            ))),
        }
    }

    pub fn sql_cmp(&self, other: &Value) -> EvalResult<Option<Ordering>> {
        match (self, other) {
            (Value::Null, _) | (_, Value::Null) => Ok(None),
            (Value::Boolean(a), Value::Boolean(b)) => Ok(Some(a.cmp(b))),
            (Value::Integer(a), Value::Integer(b)) => Ok(Some(a.cmp(b))),
            (Value::Integer(a), Value::Float(b)) => Ok(Some(cmp_f64(*a as f64, *b))),
            (Value::Float(a), Value::Integer(b)) => Ok(Some(cmp_f64(*a, *b as f64))),
            (Value::Float(a), Value::Float(b)) => Ok(Some(cmp_f64(*a, *b))),
            (Value::Text(a), Value::Text(b)) => Ok(Some(a.cmp(b))),
            (a, b) => Err(EvalError::TypeMismatch(format!(
                "cannot compare {} with {}",
                a.data_type(),
                b.data_type()
            ))),
        }
    }

    pub fn cast(&self, to: DataType) -> EvalResult<Value> {
        let invalid = || EvalError::InvalidCast {
            value: self.to_string(),
            to,
        };
        match (self, to) {
            (Value::Null, _) => Ok(Value::Null),
            (Value::Boolean(b), DataType::Boolean) => Ok(Value::Boolean(*b)),
            (Value::Integer(i), DataType::Integer) => Ok(Value::Integer(*i)),
            (Value::Integer(i), DataType::Float) => Ok(Value::Float(*i as f64)),
            (Value::Float(x), DataType::Float) => Ok(Value::Float(*x)),
            (Value::Float(x), DataType::Integer) => {
                let rounded = x.round();
                if rounded.is_finite() && rounded >= i64::MIN as f64 && rounded <= i64::MAX as f64 {
                    Ok(Value::Integer(rounded as i64))
                } else {
                    Err(invalid())
                }
            }
            (Value::Text(s), t) if t.is_textual() => Ok(Value::Text(s.clone())),
            (Value::Text(s), DataType::Integer) => {
                s.trim().parse().map(Value::Integer).map_err(|_| invalid())
            }
            (Value::Text(s), DataType::Float) => {
                s.trim().parse().map(Value::Float).map_err(|_| invalid())
            }
            (Value::Text(s), DataType::Boolean) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" => Ok(Value::Boolean(true)),
                "false" | "f" => Ok(Value::Boolean(false)),
                _ => Err(invalid()),
            },
            (v, DataType::Text) => Ok(Value::Text(v.to_string())),
            _ => Err(invalid()),
        }
    }

    pub fn from_literal(literal: &LiteralValue) -> Value {
        match literal {
            LiteralValue::Null => Value::Null,
            LiteralValue::Boolean(b) => Value::Boolean(*b),
            LiteralValue::Integer(i) => Value::Integer(*i),
            LiteralValue::Float(x) => Value::Float(*x),
            LiteralValue::String(s) => Value::Text(s.clone()),
        }
    }

    pub fn binary(op: BinaryOperator, left: &Value, right: &Value) -> EvalResult<Value> {
        match op {
            BinaryOperator::And => Ok(bool_value(and3(left.to_bool()?, right.to_bool()?))),
            BinaryOperator::Or => Ok(bool_value(or3(left.to_bool()?, right.to_bool()?))),
            op if op.is_comparison() => {
                let ordering = left.sql_cmp(right)?;
                Ok(match ordering {
                    None => Value::Null,
                    Some(o) => Value::Boolean(match op {
                        BinaryOperator::Eq => o == Ordering::Equal,
                        BinaryOperator::NotEq => o != Ordering::Equal,
                        BinaryOperator::Lt => o == Ordering::Less,
                        BinaryOperator::LtEq => o != Ordering::Greater,
                        BinaryOperator::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    }),
                })
            }
            BinaryOperator::Concat => match (left, right) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (l, r) => Ok(Value::Text(format!("{l}{r}"))),
            },
            op => arithmetic(op, left, right),
        }
    }

    pub fn unary(op: UnaryOperator, value: &Value) -> EvalResult<Value> {
        match (op, value) {
            (_, Value::Null) => Ok(Value::Null),
            (UnaryOperator::Not, v) => Ok(bool_value(v.to_bool()?.map(|b| !b))),
            (UnaryOperator::Minus, Value::Integer(i)) => i
                .checked_neg()
                .map(Value::Integer)
                .ok_or_else(|| EvalError::Overflow(format!("-({i})"))),
            (UnaryOperator::Minus, Value::Float(x)) => Ok(Value::Float(-x)),
            (UnaryOperator::Plus, v @ (Value::Integer(_) | Value::Float(_))) => Ok(v.clone()),
            (op, v) => Err(EvalError::TypeMismatch(format!(
                "operator {} cannot be applied to {}",
                op.symbol().trim(),
                v.data_type()
            ))),
        }
    }
}

pub fn and3(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub fn or3(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

pub fn bool_value(b: Option<bool>) -> Value {
    b.map_or(Value::Null, Value::Boolean)
}

fn arithmetic(op: BinaryOperator, left: &Value, right: &Value) -> EvalResult<Value> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Integer(a), Value::Integer(b)) => integer_arithmetic(op, *a, *b),
        (Value::Integer(a), Value::Float(b)) => float_arithmetic(op, *a as f64, *b),
        (Value::Float(a), Value::Integer(b)) => float_arithmetic(op, *a, *b as f64),
        (Value::Float(a), Value::Float(b)) => float_arithmetic(op, *a, *b),
        (l, r) => Err(EvalError::TypeMismatch(format!(
            "operator {} cannot be applied to {} and {}",
            op.symbol(),
            l.data_type(),
            r.data_type()
        ))),
    }
}

fn integer_arithmetic(op: BinaryOperator, a: i64, b: i64) -> EvalResult<Value> {
    if matches!(op, BinaryOperator::Divide | BinaryOperator::Modulo) && b == 0 {
        return Err(EvalError::DivisionByZero);
    }
    let result = match op {
        BinaryOperator::Plus => a.checked_add(b),
        BinaryOperator::Minus => a.checked_sub(b),
        BinaryOperator::Multiply => a.checked_mul(b),
        BinaryOperator::Divide => a.checked_div(b),
        BinaryOperator::Modulo => a.checked_rem(b),
        other => {
            return Err(EvalError::TypeMismatch(format!(
                "{} is not an arithmetic operator",
                other.symbol()
            )));
        }
    };
    result
        .map(Value::Integer)
        .ok_or_else(|| EvalError::Overflow(format!("{a} {} {b}", op.symbol())))
}

fn float_arithmetic(op: BinaryOperator, a: f64, b: f64) -> EvalResult<Value> {
    if matches!(op, BinaryOperator::Divide | BinaryOperator::Modulo) && b == 0.0 {
        return Err(EvalError::DivisionByZero);
    }
    let result = match op {
        BinaryOperator::Plus => a + b,
        BinaryOperator::Minus => a - b,
        BinaryOperator::Multiply => a * b,
        BinaryOperator::Divide => a / b,
        BinaryOperator::Modulo => a % b,
        other => {
            return Err(EvalError::TypeMismatch(format!(
                "{} is not an arithmetic operator",
                other.symbol()
            )));
        }
    };
    if result.is_finite() {
        Ok(Value::Float(result))
    } else {
        Err(EvalError::Overflow(format!("{a} {} {b}", op.symbol())))
    }
}

pub fn like_match(value: &str, pattern: &str, case_insensitive: bool) -> bool {
    let fold = |s: &str| -> Vec<char> {
        if case_insensitive {
            s.to_lowercase().chars().collect()
        } else {
            s.chars().collect()
        }
    };
    let text = fold(value);
    let pat = fold(pattern);

    let (mut t, mut p) = (0usize, 0usize);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        match pat.get(p) {
            Some('%') => {
                backtrack = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '_' || c == text[t] => {
                t += 1;
                p += 1;
            }
            _ => match backtrack {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    backtrack = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pat[p..].iter().all(|&c| c == '%')
}

fn cmp_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b)
        .unwrap_or_else(|| a.is_nan().cmp(&b.is_nan()))
}

fn canonical_f64_bits(x: f64) -> u64 {
    if x == 0.0 {
        0
    } else if x.is_nan() {
        f64::NAN.to_bits()
    } else {
        x.to_bits()
    }
}

impl Value {
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Boolean(_) => 1,
            Value::Integer(_) | Value::Float(_) => 2,
            Value::Text(_) => 3,
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Boolean(a), Value::Boolean(b)) => a == b,
            (Value::Integer(a), Value::Integer(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => cmp_f64(*a, *b) == Ordering::Equal,
            (Value::Text(a), Value::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Boolean(b) => b.hash(state),
            Value::Integer(i) => i.hash(state),
            Value::Float(x) => canonical_f64_bits(*x).hash(state),
            Value::Text(s) => s.hash(state),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
            (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => cmp_f64(*a, *b),
            (Value::Integer(a), Value::Float(b)) => cmp_f64(*a as f64, *b).then(Ordering::Less),
            (Value::Float(a), Value::Integer(b)) => cmp_f64(*a, *b as f64).then(Ordering::Greater),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) => f.write_str(s),
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Integer(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Float(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Boolean(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Text(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Text(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_valued_and_or() {
        assert_eq!(and3(None, Some(false)), Some(false));
        assert_eq!(and3(None, Some(true)), None);
        assert_eq!(or3(None, Some(true)), Some(true));
        assert_eq!(or3(None, Some(false)), None);
    }

    #[test]
    fn comparison_with_null_is_null() {
        let v = Value::binary(BinaryOperator::Eq, &Value::Null, &Value::Integer(1)).unwrap();
        assert!(v.is_null());
    }

    #[test]
    fn mixed_numeric_comparison() {
        let v = Value::binary(BinaryOperator::Lt, &Value::Integer(1), &Value::Float(1.5)).unwrap();
        assert_eq!(v, Value::Boolean(true));
    }

    #[test]
    fn integer_overflow_is_an_error() {
        let err = Value::binary(
            BinaryOperator::Plus,
            &Value::Integer(i64::MAX),
            &Value::Integer(1),
        )
        .unwrap_err();
        assert!(matches!(err, EvalError::Overflow(_)));
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let err = Value::binary(
            BinaryOperator::Divide,
            &Value::Integer(1),
            &Value::Integer(0),
        )
        .unwrap_err();
        assert_eq!(err, EvalError::DivisionByZero);
    }

    #[test]
    fn integer_division_truncates() {
        let v = Value::binary(
            BinaryOperator::Divide,
            &Value::Integer(7),
            &Value::Integer(2),
        )
        .unwrap();
        assert_eq!(v, Value::Integer(3));
    }

    #[test]
    fn like_patterns() {
        assert!(like_match("alice", "a%", false));
        assert!(like_match("alice", "%ic%", false));
        assert!(like_match("alice", "_lice", false));
        assert!(!like_match("alice", "_ice", false));
        assert!(like_match("ALICE", "a%e", true));
        assert!(!like_match("ALICE", "a%e", false));
        assert!(like_match("", "%", false));
        assert!(like_match("abcabc", "%abc", false));
        assert!(!like_match("abc", "abcd%", false));
    }

    #[test]
    fn total_order_groups_zero_and_negative_zero() {
        assert_eq!(Value::Float(0.0), Value::Float(-0.0));
        let mut a = std::collections::hash_map::DefaultHasher::new();
        let mut b = std::collections::hash_map::DefaultHasher::new();
        Value::Float(0.0).hash(&mut a);
        Value::Float(-0.0).hash(&mut b);
        assert_eq!(a.finish(), b.finish());
    }

    #[test]
    fn total_order_is_consistent_with_eq_for_mixed_numbers() {
        assert_ne!(Value::Integer(1), Value::Float(1.0));
        assert_eq!(Value::Integer(1).cmp(&Value::Float(1.0)), Ordering::Less);
        assert_eq!(Value::Float(1.0).cmp(&Value::Integer(1)), Ordering::Greater);
        assert!(Value::Null < Value::Integer(i64::MIN));
    }

    #[test]
    fn cast_rules() {
        assert_eq!(
            Value::Integer(3).cast(DataType::Float).unwrap(),
            Value::Float(3.0)
        );
        assert_eq!(
            Value::Text("42".into()).cast(DataType::Integer).unwrap(),
            Value::Integer(42)
        );
        assert!(Value::Text("x".into()).cast(DataType::Integer).is_err());
        assert!(Value::Null.cast(DataType::Integer).unwrap().is_null());
    }
}
