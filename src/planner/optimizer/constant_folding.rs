//! If an expression has no column reference then we can say that the it has the
//! same value for every row, so instead of computing the expression every row
//! we can compute it once and optimize the query execution with less computation.

use crate::{
    planner::{
        error::PlanResult,
        expr::{AggregateCall, ScalarExpr, SortKey},
        logical::LogicalPlan,
        optimizer::OptimizerRule,
        physical_expr::PhysExpr,
        schema::Schema,
    },
    sql::{BinaryOperator, JoinType},
};

pub struct ConstantFolding;

impl OptimizerRule for ConstantFolding {
    fn name(&self) -> &'static str {
        "constant_folding"
    }

    fn rewrite(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan> {
        plan.transform_up(&mut |node| simplify_node(fold_node_exprs(node)?))
    }
}

pub fn fold_expr(expr: ScalarExpr) -> PlanResult<ScalarExpr> {
    expr.transform_up(&mut |e| Ok(simplify_boolean(evaluate_constant(e))))
}

pub fn simplify_boolean(expr: ScalarExpr) -> ScalarExpr {
    let ScalarExpr::Binary { op, left, right } = expr else {
        return expr;
    };

    // `false AND x`, `x AND false` => false
    // `true OR x`, `x OR true` => true
    // `true AND x`, `false OR x` => x
    // `x AND true`, `x OR false` => x
    match (op, left.as_literal_bool(), right.as_literal_bool()) {
        (BinaryOperator::And, Some(Some(false)), _)
        | (BinaryOperator::And, _, Some(Some(false))) => ScalarExpr::literal(false),
        (BinaryOperator::Or, Some(Some(true)), _) | (BinaryOperator::Or, _, Some(Some(true))) => {
            ScalarExpr::literal(true)
        }
        (BinaryOperator::And, Some(Some(true)), _) | (BinaryOperator::Or, Some(Some(false)), _) => {
            *right
        }
        (BinaryOperator::And, _, Some(Some(true))) | (BinaryOperator::Or, _, Some(Some(false))) => {
            *left
        }
        _ => ScalarExpr::Binary { op, left, right },
    }
}

fn evaluate_constant(expr: ScalarExpr) -> ScalarExpr {
    if matches!(expr, ScalarExpr::Literal(_)) || !expr.is_constant() {
        return expr;
    }
    let evaluated = PhysExpr::lower(&expr, &Schema::empty())
        .ok()
        .and_then(|phys| phys.eval(&[]).ok());
    match evaluated {
        Some(value) => ScalarExpr::Literal(value),
        None => expr,
    }
}
fn fold_node_exprs(plan: LogicalPlan) -> PlanResult<LogicalPlan> {
    Ok(match plan {
        LogicalPlan::Filter { input, predicate } => LogicalPlan::Filter {
            input,
            predicate: fold_expr(predicate)?,
        },
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => LogicalPlan::Project {
            input,
            exprs: fold_all(exprs)?,
            schema,
        },
        LogicalPlan::Values { rows, schema } => LogicalPlan::Values {
            rows: rows.into_iter().map(fold_all).collect::<PlanResult<_>>()?,
            schema,
        },
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            schema,
        } => LogicalPlan::Join {
            left,
            right,
            join_type,
            on: on.map(fold_expr).transpose()?,
            schema,
        },
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => LogicalPlan::Aggregate {
            input,
            group_by: fold_all(group_by)?,
            aggregates: aggregates
                .into_iter()
                .map(|a| {
                    Ok(AggregateCall {
                        arg: a.arg.map(fold_expr).transpose()?,
                        ..a
                    })
                })
                .collect::<PlanResult<_>>()?,
            schema,
        },
        LogicalPlan::Sort { input, keys } => LogicalPlan::Sort {
            input,
            keys: keys
                .into_iter()
                .map(|k| {
                    Ok(SortKey {
                        expr: fold_expr(k.expr)?,
                        ..k
                    })
                })
                .collect::<PlanResult<_>>()?,
        },
        other => other,
    })
}

fn fold_all(exprs: Vec<ScalarExpr>) -> PlanResult<Vec<ScalarExpr>> {
    exprs.into_iter().map(fold_expr).collect()
}

fn simplify_node(plan: LogicalPlan) -> PlanResult<LogicalPlan> {
    let empty = || LogicalPlan::empty(plan.schema().clone());
    Ok(match &plan {
        LogicalPlan::Filter { predicate, .. } => match predicate.as_literal_bool() {
            Some(Some(true)) => match plan {
                LogicalPlan::Filter { input, .. } => *input,
                other => other,
            },
            Some(_) => empty(),
            None if plan.children()[0].is_empty_relation() => empty(),
            None => plan,
        },
        LogicalPlan::Limit { limit: Some(0), .. } => empty(),
        LogicalPlan::Project { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Limit { input, .. }
            if input.is_empty_relation() =>
        {
            empty()
        }
        LogicalPlan::Aggregate {
            input, group_by, ..
        } if !group_by.is_empty() && input.is_empty_relation() => empty(),
        LogicalPlan::Join {
            left,
            right,
            join_type,
            ..
        } => {
            let left_empty = left.is_empty_relation();
            let right_empty = right.is_empty_relation();
            let produces_nothing = match join_type {
                JoinType::Inner | JoinType::Cross => left_empty || right_empty,
                JoinType::Left => left_empty,
                JoinType::Right => right_empty,
                JoinType::Full => left_empty && right_empty,
            };
            if produces_nothing { empty() } else { plan }
        }
        _ => plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::schema::ColumnId;
    use crate::planner::types::DataType;
    use crate::planner::value::Value;

    fn column() -> ScalarExpr {
        ScalarExpr::column(ColumnId(0), "t.a", DataType::Boolean)
    }

    #[test]
    fn folds_arithmetic() {
        let expr = ScalarExpr::binary(
            BinaryOperator::Plus,
            ScalarExpr::literal(1i64),
            ScalarExpr::binary(
                BinaryOperator::Multiply,
                ScalarExpr::literal(2i64),
                ScalarExpr::literal(3i64),
            ),
        );
        assert_eq!(fold_expr(expr).unwrap(), ScalarExpr::literal(7i64));
    }

    #[test]
    fn keeps_failing_constant_expressions_for_runtime() {
        let expr = ScalarExpr::binary(
            BinaryOperator::Divide,
            ScalarExpr::literal(1i64),
            ScalarExpr::literal(0i64),
        );
        assert_eq!(fold_expr(expr.clone()).unwrap(), expr);
    }

    #[test]
    fn simplifies_boolean_identities() {
        let and_true = ScalarExpr::and(column(), ScalarExpr::literal(true));
        assert_eq!(fold_expr(and_true).unwrap(), column());

        let and_false = ScalarExpr::and(column(), ScalarExpr::literal(false));
        assert_eq!(fold_expr(and_false).unwrap(), ScalarExpr::literal(false));

        let or_true = ScalarExpr::binary(BinaryOperator::Or, ScalarExpr::literal(true), column());
        assert_eq!(fold_expr(or_true).unwrap(), ScalarExpr::literal(true));
    }

    #[test]
    fn null_and_column_is_not_simplified() {
        let expr = ScalarExpr::and(ScalarExpr::Literal(Value::Null), column());
        assert_eq!(fold_expr(expr.clone()).unwrap(), expr);
    }
}
