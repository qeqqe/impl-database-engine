use crate::{
    planner::{
        error::PlanResult, expr::ScalarExpr, logical::LogicalPlan, optimizer::OptimizerRule,
    },
    sql::JoinType,
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Side {
    Left,
    Right,
    Join,
    Above,
}

pub struct PredicatePushdown;

impl OptimizerRule for PredicatePushdown {
    fn name(&self) -> &'static str {
        "predicate_pushdown"
    }

    fn rewrite(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan> {
        push_down(plan, Vec::new())
    }
}

fn apply(plan: LogicalPlan, predicates: Vec<ScalarExpr>) -> LogicalPlan {
    match ScalarExpr::conjunction(predicates) {
        Some(predicate) => LogicalPlan::filter(plan, predicate),
        None => plan,
    }
}

fn push_down(plan: LogicalPlan, mut predicates: Vec<ScalarExpr>) -> PlanResult<LogicalPlan> {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            predicates.extend(predicate.split_conjunction());
            push_down(*input, predicates)
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let (below, above): (Vec<_>, Vec<_>) = predicates
                .into_iter()
                .partition(|p| p.is_bound_by(input.schema()));
            let input = push_down(*input, below)?;
            Ok(apply(
                LogicalPlan::Project {
                    input: Box::new(input),
                    exprs,
                    schema,
                },
                above,
            ))
        }
        LogicalPlan::Sort { input, keys } => Ok(LogicalPlan::Sort {
            input: Box::new(push_down(*input, predicates)?),
            keys,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(push_down(*input, predicates)?),
        }),
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => {
            let (below, above): (Vec<_>, Vec<_>) = predicates.into_iter().partition(|p| {
                !group_by.is_empty() && !p.is_constant() && p.is_bound_by(input.schema())
            });
            let input = push_down(*input, below)?;
            Ok(apply(
                LogicalPlan::Aggregate {
                    input: Box::new(input),
                    group_by,
                    aggregates,
                    schema,
                },
                above,
            ))
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            ..
        } => push_into_join(*left, *right, join_type, on, predicates),
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => {
            let input = push_down(*input, Vec::new())?;
            Ok(apply(
                LogicalPlan::Limit {
                    input: Box::new(input),
                    limit,
                    offset,
                },
                predicates,
            ))
        }
        LogicalPlan::SetOperation {
            op,
            all,
            left,
            right,
            schema,
        } => {
            let left = push_down(*left, Vec::new())?;
            let right = push_down(*right, Vec::new())?;
            Ok(apply(
                LogicalPlan::SetOperation {
                    op,
                    all,
                    left: Box::new(left),
                    right: Box::new(right),
                    schema,
                },
                predicates,
            ))
        }
        leaf @ (LogicalPlan::Scan { .. } | LogicalPlan::Values { .. }) => {
            Ok(apply(leaf, predicates))
        }
    }
}

fn push_into_join(
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    on: Option<ScalarExpr>,
    predicates: Vec<ScalarExpr>,
) -> PlanResult<LogicalPlan> {
    let mut to_left = Vec::new();
    let mut to_right = Vec::new();
    let mut join_conditions = Vec::new();
    let mut above = Vec::new();

    let route = |p: &ScalarExpr, from_where: bool| -> Side {
        let in_left = p.is_bound_by(left.schema());
        let in_right = p.is_bound_by(right.schema());
        match (join_type, from_where) {
            (JoinType::Inner | JoinType::Cross, _) if in_left => Side::Left,
            (JoinType::Inner | JoinType::Cross, _) if in_right => Side::Right,
            (JoinType::Inner | JoinType::Cross, _) => Side::Join,
            (JoinType::Left, true) if in_left => Side::Left,
            (JoinType::Right, true) if in_right => Side::Right,
            (_, true) => Side::Above,
            (JoinType::Left, false) if in_right => Side::Right,
            (JoinType::Right, false) if in_left => Side::Left,
            (_, false) => Side::Join,
        }
    };

    let where_routes: Vec<(ScalarExpr, Side)> = predicates
        .into_iter()
        .map(|p| {
            let side = route(&p, true);
            (p, side)
        })
        .collect();
    let on_routes: Vec<(ScalarExpr, Side)> = on
        .map(ScalarExpr::split_conjunction)
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let side = route(&p, false);
            (p, side)
        })
        .collect();

    for (predicate, side) in where_routes.into_iter().chain(on_routes) {
        match side {
            Side::Left => to_left.push(predicate),
            Side::Right => to_right.push(predicate),
            Side::Join => join_conditions.push(predicate),
            Side::Above => above.push(predicate),
        }
    }

    let join_type = if join_type == JoinType::Cross && !join_conditions.is_empty() {
        JoinType::Inner
    } else {
        join_type
    };

    let left = push_down(left, to_left)?;
    let right = push_down(right, to_right)?;
    let join = LogicalPlan::join(
        left,
        right,
        join_type,
        ScalarExpr::conjunction(join_conditions),
    );
    Ok(apply(join, above))
}
