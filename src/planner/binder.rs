use crate::planner::expr::{AggregateFunction, ScalarFunction};
use crate::planner::schema::ColumnId;
use crate::planner::types::DataType;
use crate::planner::value::Value;
use crate::sql::{BinaryOperator, Expr, FunctionArgs, UnaryOperator};

use super::catalog::Catalog;
use super::error::{PlanError, PlanResult};
use super::expr::{AggregateCall, ScalarExpr};
use super::schema::{ColumnIdGenerator, Field};

pub struct Binder<'a> {
    catalog: &'a Catalog,
    ids: ColumnIdGenerator,
}

struct Relation {
    alias: String,
    fields: Vec<Field>,
}

struct Scope {
    relations: Vec<Relation>,
}

impl Scope {
    fn add(&mut self, alias: &str, fields: Vec<Field>) -> PlanResult<()> {
        if self
            .relations
            .iter()
            .any(|f| f.alias.eq_ignore_ascii_case(alias))
        {
            return Err(PlanError::DuplicateAlias(alias.to_string()));
        }

        self.relations.push(Relation {
            alias: alias.to_string(),
            fields,
        });
        Ok(())
    }

    fn relation(&self, alias: &str) -> PlanResult<&Relation> {
        self.relations
            .iter()
            .find(|f| f.alias.eq_ignore_ascii_case(alias))
            .ok_or_else(|| PlanError::AliasNotFound(alias.to_string()))
    }

    fn resolve(&self, table: Option<&str>, column: &str) -> PlanResult<&Field> {
        let not_found = || PlanError::ColumnNotFound {
            table: table.map(String::from),
            column: column.to_string(),
        };

        match table {
            Some(t) => self
                .relation(t)?
                .fields
                .iter()
                .find(|f| f.name.eq_ignore_ascii_case(column))
                .ok_or_else(not_found),
            None => {
                let mut matches = self.relations.iter().filter_map(|r| {
                    r.fields
                        .iter()
                        .find(|f| f.name.eq_ignore_ascii_case(column))
                        .map(|f| (r, f))
                });

                let (_, field) = matches.next().ok_or_else(not_found)?;
                let others: Vec<&Relation> = matches.map(|(r, _)| r).collect();
                if others.is_empty() {
                    Ok(field)
                } else {
                    let mut candidates = vec![field.qualifier.clone().unwrap_or_default()];
                    candidates.extend(others.iter().map(|r| r.alias.clone()));
                    Err(PlanError::AmbiguousColumn {
                        column: column.to_string(),
                        candidates,
                    })
                }
            }
        }
    }

    fn all_fields(&self) -> impl Iterator<Item = &Field> {
        self.relations.iter().flat_map(|r| r.fields.iter())
    }
}

fn column_ref(field: &Field) -> ScalarExpr {
    ScalarExpr::column(field.id, field.qualified_name(), field.data_type)
}

#[derive(Debug, Default)]
struct AggregateSet {
    calls: Vec<AggregateCall>,
    fields: Vec<Field>,
}

impl AggregateSet {
    fn register(&mut self, call: AggregateCall, ids: &mut ColumnIdGenerator) -> ScalarExpr {
        if let Some(i) = self.calls.iter().position(|c| c == &call) {
            return column_ref(&self.fields[i]);
        }
        let field = Field {
            id: ids.next_id(),
            qualifier: None,
            name: call.to_string(),
            data_type: call.data_type(),
            nullable: call.nullable(),
        };
        let expr = column_ref(&field);
        self.calls.push(call);
        self.fields.push(field);
        expr
    }
}

enum AggregateMode<'s> {
    Forbidden(&'static str),
    Allowed(&'s mut AggregateSet),
    InsideAggregate(String),
}

struct ExprBinder<'s> {
    scope: &'s Scope,
    ids: &'s ColumnIdGenerator,
    mode: AggregateMode<'s>,
}

impl ExprBinder<'_> {
    fn bind(&mut self, expr: &Expr) -> PlanResult<ScalarExpr> {
        match expr {
            Expr::Column { table, name } => {
                Ok(column_ref(self.scope.resolve(table.as_deref(), name)?))
            }
            Expr::Literal(lit) => Ok(ScalarExpr::Literal(Value::from_literal(lit))),
            Expr::Nested(inner) => self.bind(inner),
            Expr::UnaryOp { op, expr: inner } => {
                let inner = self.bind(inner)?;
                let t = inner.data_type();
                let valid = match op {
                    UnaryOperator::Not => t.is_boolean(),
                    UnaryOperator::Minus | UnaryOperator::Plus => t.is_numeric() || t.is_null(),
                };
                if !valid {
                    return Err(PlanError::TypeMismatch(format!(
                        "operator {} cannot be applied to {t} in {expr}",
                        op.symbol().trim()
                    )));
                }
                Ok(ScalarExpr::Unary {
                    op: *op,
                    expr: Box::new(inner),
                })
            }
            Expr::BinaryOp { left, op, right } => {
                let l = self.bind(left)?;
                let r = self.bind(right)?;
                let (lt, rt) = (l.data_type(), r.data_type());
                let valid = if op.is_logical() {
                    lt.is_boolean() && rt.is_boolean()
                } else if op.is_comparison() {
                    lt.is_comparable_with(rt)
                } else if op.is_arithmetic() {
                    lt.numeric_result(rt).is_some()
                } else {
                    true
                };
                if !valid {
                    return Err(PlanError::TypeMismatch(format!(
                        "operator {} cannot be applied to {lt} and {rt} in {expr}",
                        op.symbol()
                    )));
                }
                Ok(ScalarExpr::binary(*op, l, r))
            }
            Expr::IsNull {
                expr: inner,
                negated,
            } => Ok(ScalarExpr::IsNull {
                expr: Box::new(self.bind(inner)?),
                negated: *negated,
            }),
            Expr::InList {
                expr: inner,
                list,
                negated,
            } => {
                let inner = self.bind(inner)?;
                let list = list
                    .iter()
                    .map(|item| self.bind(item))
                    .collect::<PlanResult<Vec<_>>>()?;
                let t = inner.data_type();
                if let Some(bad) = list.iter().find(|i| !t.is_comparable_with(i.data_type())) {
                    return Err(PlanError::TypeMismatch(format!(
                        "IN list item {bad} of type {} is not comparable with {t}",
                        bad.data_type()
                    )));
                }
                Ok(ScalarExpr::InList {
                    expr: Box::new(inner),
                    list,
                    negated: *negated,
                })
            }
            Expr::Between {
                expr: inner,
                low,
                high,
                negated,
            } => {
                let inner = self.bind(inner)?;
                let low = self.bind(low)?;
                let high = self.bind(high)?;
                let t = inner.data_type();
                if !t.is_comparable_with(low.data_type()) || !t.is_comparable_with(high.data_type())
                {
                    return Err(PlanError::TypeMismatch(format!(
                        "BETWEEN bounds are not comparable with {t} in {expr}"
                    )));
                }
                let range = ScalarExpr::and(
                    ScalarExpr::binary(crate::sql::BinaryOperator::GtEq, inner.clone(), low),
                    ScalarExpr::binary(crate::sql::BinaryOperator::LtEq, inner, high),
                );
                Ok(if *negated {
                    ScalarExpr::negate(range)
                } else {
                    range
                })
            }
            Expr::Like {
                expr: inner,
                pattern,
                negated,
                case_insensitive,
            } => {
                let inner = self.bind(inner)?;
                let t = inner.data_type();
                if !(t.is_textual() || t.is_null()) {
                    return Err(PlanError::TypeMismatch(format!(
                        "LIKE requires a text operand, got {t} in {expr}"
                    )));
                }
                Ok(ScalarExpr::Like {
                    expr: Box::new(inner),
                    pattern: pattern.clone(),
                    negated: *negated,
                    case_insensitive: *case_insensitive,
                })
            }
            Expr::Function { name, args } => self.bind_function(expr, name, args),
        }
    }

    fn bind_function(
        &mut self,
        expr: &Expr,
        name: &str,
        args: &FunctionArgs,
    ) -> PlanResult<ScalarExpr> {
        todo!()
    }

    fn bind_aggregate(
        &mut self,
        expr: &Expr,
        func: AggregateFunction,
        args: &FunctionArgs,
    ) -> PlanResult<ScalarExpr> {
        todo!()
    }
}
