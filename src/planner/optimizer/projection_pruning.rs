use std::collections::BTreeSet;

use crate::planner::{
    error::PlanResult,
    expr::ScalarExpr,
    logical::LogicalPlan,
    optimizer::OptimizerRule,
    schema::{ColumnId, Field, Schema},
};

pub struct ProjectionPruning;

impl OptimizerRule for ProjectionPruning {
    fn name(&self) -> &'static str {
        "projection_pruning"
    }

    fn rewrite(&self, plan: LogicalPlan) -> PlanResult<LogicalPlan> {
        let required: BTreeSet<ColumnId> = plan.schema().ids().collect();
        prune(plan, &required)
    }
}

fn with_columns<'a>(
    required: &BTreeSet<ColumnId>,
    exprs: impl IntoIterator<Item = &'a ScalarExpr>,
) -> BTreeSet<ColumnId> {
    let mut out = required.clone();
    for expr in exprs {
        expr.collect_column_ids(&mut out);
    }
    out
}

fn prune(plan: LogicalPlan, required: &BTreeSet<ColumnId>) -> PlanResult<LogicalPlan> {
    match plan {
        LogicalPlan::Scan {
            table,
            alias,
            projection,
            schema,
        } => {
            let (projection, fields): (Vec<usize>, Vec<Field>) = projection
                .into_iter()
                .zip(schema.into_fields())
                .filter(|(_, f)| required.contains(&f.id))
                .unzip();
            Ok(LogicalPlan::Scan {
                table,
                alias,
                projection,
                schema: Schema::new(fields),
            })
        }
        values @ LogicalPlan::Values { .. } => Ok(values),
        LogicalPlan::Filter { input, predicate } => {
            let child_required = with_columns(required, [&predicate]);
            Ok(LogicalPlan::Filter {
                input: Box::new(prune(*input, &child_required)?),
                predicate,
            })
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let (exprs, fields): (Vec<ScalarExpr>, Vec<Field>) = exprs
                .into_iter()
                .zip(schema.into_fields())
                .filter(|(_, f)| required.contains(&f.id))
                .unzip();
            let child_required = with_columns(&BTreeSet::new(), &exprs);
            let input = prune(*input, &child_required)?;
            let schema = Schema::new(fields);
            if is_identity(&exprs, &schema, input.schema()) {
                Ok(input)
            } else {
                Ok(LogicalPlan::Project {
                    input: Box::new(input),
                    exprs,
                    schema,
                })
            }
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            ..
        } => {
            let needed = with_columns(required, on.iter());
            let left = prune(*left, &needed)?;
            let right = prune(*right, &needed)?;
            Ok(LogicalPlan::join(left, right, join_type, on))
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => {
            let group_count = group_by.len();
            let mut fields = schema.into_fields();
            let aggregate_fields = fields.split_off(group_count);
            let (aggregates, aggregate_fields): (Vec<_>, Vec<_>) = aggregates
                .into_iter()
                .zip(aggregate_fields)
                .filter(|(_, f)| required.contains(&f.id))
                .unzip();
            fields.extend(aggregate_fields);

            let child_required = with_columns(
                &BTreeSet::new(),
                group_by
                    .iter()
                    .chain(aggregates.iter().filter_map(|a| a.arg.as_ref())),
            );
            Ok(LogicalPlan::Aggregate {
                input: Box::new(prune(*input, &child_required)?),
                group_by,
                aggregates,
                schema: Schema::new(fields),
            })
        }
        LogicalPlan::Distinct { input } => {
            let all: BTreeSet<ColumnId> = input.schema().ids().collect();
            Ok(LogicalPlan::Distinct {
                input: Box::new(prune(*input, &all)?),
            })
        }
        LogicalPlan::Sort { input, keys } => {
            let child_required = with_columns(required, keys.iter().map(|k| &k.expr));
            Ok(LogicalPlan::Sort {
                input: Box::new(prune(*input, &child_required)?),
                keys,
            })
        }
        LogicalPlan::Limit {
            input,
            limit,
            offset,
        } => Ok(LogicalPlan::Limit {
            input: Box::new(prune(*input, required)?),
            limit,
            offset,
        }),
        LogicalPlan::SetOperation {
            op,
            all,
            left,
            right,
            schema,
        } => {
            let left_all: BTreeSet<ColumnId> = left.schema().ids().collect();
            let right_all: BTreeSet<ColumnId> = right.schema().ids().collect();
            Ok(LogicalPlan::SetOperation {
                op,
                all,
                left: Box::new(prune(*left, &left_all)?),
                right: Box::new(prune(*right, &right_all)?),
                schema,
            })
        }
    }
}

fn is_identity(exprs: &[ScalarExpr], schema: &Schema, input: &Schema) -> bool {
    schema == input
        && exprs
            .iter()
            .zip(input.fields())
            .all(|(e, f)| matches!(e, ScalarExpr::Column(c) if c.id == f.id))
}
