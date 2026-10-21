use core::fmt;
use std::collections::HashSet;

use crate::planner::{
    error::PlanError,
    expr::ScalarExpr,
    logical::{LogicalPlan, UpdateAssignment},
    optimizer::{Optimizer, constant_folding::fold_expr},
    value::{Row, Value},
};

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum SubqueryKind {
    Scalar,
    Exists,
    In,
}

impl fmt::Display for SubqueryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SubqueryKind::Scalar => "SCALAR",
            SubqueryKind::Exists => "EXISTS",
            SubqueryKind::In => "IN",
        })
    }
}

pub fn resolve_assignments<E, F>(
    assignments: Vec<UpdateAssignment>,
    run: &mut F,
) -> Result<Vec<UpdateAssignment>, E>
where
    E: From<PlanError>,
    F: FnMut(SubqueryKind, LogicalPlan) -> Result<Vec<Row>, E>,
{
    assignments
        .into_iter()
        .map(|a| {
            if !a.value.contains_subquery() {
                return Ok(a);
            }
            let value = fold_expr(resolve_expr(a.value, run)?)?;
            Ok(UpdateAssignment {
                column: a.column,
                value,
            })
        })
        .collect()
}

pub fn resolve_plan<E, F>(plan: LogicalPlan, run: &mut F) -> Result<LogicalPlan, E>
where
    E: From<PlanError>,
    F: FnMut(SubqueryKind, LogicalPlan) -> Result<Vec<Row>, E>,
{
    if !plan.contains_subquery() {
        return Ok(plan);
    }

    let resolved = plan.try_transform_up(&mut |node| node.map_exprs(|e| resolve_expr(e, run)))?;
    Ok(Optimizer::default().optimize(resolved)?)
}
fn resolve_node<E, F>(expr: ScalarExpr, run: &mut F) -> Result<ScalarExpr, E>
where
    E: From<PlanError>,
    F: FnMut(SubqueryKind, LogicalPlan) -> Result<Vec<Row>, E>,
{
    match expr {
        ScalarExpr::Subquery { plan } => {
            let mut rows = run_subquery(SubqueryKind::Scalar, *plan, Some(2), run)?;
            if rows.len() > 1 {
                return Err(PlanError::InvalidSubquery(
                    "more than one row returned by a subquery used as an expression".into(),
                )
                .into());
            }
            let value = rows
                .pop()
                .and_then(|row| row.into_iter().next())
                .unwrap_or(Value::Null);
            Ok(ScalarExpr::Literal(value))
        }
        ScalarExpr::Exists { plan, negated } => {
            let rows = run_subquery(SubqueryKind::Exists, *plan, Some(1), run)?;
            Ok(ScalarExpr::literal(rows.is_empty() == negated))
        }
        ScalarExpr::InSubquery {
            expr,
            plan,
            negated,
        } => {
            let rows = run_subquery(SubqueryKind::In, *plan, None, run)?;
            let mut seen = HashSet::with_capacity(rows.len());
            let list = rows
                .into_iter()
                .filter_map(|row| row.into_iter().next())
                .filter(|value| seen.insert(value.clone()))
                .map(ScalarExpr::Literal)
                .collect();
            Ok(ScalarExpr::InList {
                expr,
                list,
                negated,
            })
        }
        other => Ok(other),
    }
}

pub fn resolve_expr<E, F>(expr: ScalarExpr, run: &mut F) -> Result<ScalarExpr, E>
where
    E: From<PlanError>,
    F: FnMut(SubqueryKind, LogicalPlan) -> Result<Vec<Row>, E>,
{
    if !expr.contains_subquery() {
        return Ok(expr);
    }

    expr.try_transform_up(&mut |e| resolve_node(e, run))
}

fn run_subquery<E, F>(
    kind: SubqueryKind,
    plan: LogicalPlan,
    limit: Option<usize>,
    run: &mut F,
) -> Result<Vec<Row>, E>
where
    E: From<PlanError>,
    F: FnMut(SubqueryKind, LogicalPlan) -> Result<Vec<Row>, E>,
{
    let plan = resolve_plan(plan, run)?;
    let plan = match limit {
        Some(limit) => LogicalPlan::Limit {
            input: Box::new(plan),
            limit: Some(limit),
            offset: 0,
        },
        None => plan,
    };
    let plan = Optimizer::default().optimize(plan)?;
    run(kind, plan)
}
