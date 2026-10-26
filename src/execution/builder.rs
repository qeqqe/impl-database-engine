use crate::execution::operators::{
    DistinctExec, FilterExec, HashAggregateExec, HashJoinExec, LimitExec, NestedLoopJoinExec,
    ProjectExec, ScanExec, ScanKind, SetOpExec, SortExec, TopNExec, ValuesExec,
};

use crate::{
    execution::{operator::BoxedOperator, source::TableSource},
    planner::physical::PhysicalPlan,
};

pub type BuildHook<'h, 'a> = dyn FnMut(&PhysicalPlan, BoxedOperator<'a>) -> BoxedOperator<'a> + 'h;

pub fn build_operator<'a>(plan: &PhysicalPlan, source: &'a dyn TableSource) -> BoxedOperator<'a> {
    build_operator_with(plan, source, &mut |_, operator| operator)
}

pub fn build_operator_with<'a>(
    plan: &PhysicalPlan,
    source: &'a dyn TableSource,
    hook: &mut BuildHook<'_, 'a>,
) -> BoxedOperator<'a> {
    let operator = build_node(plan, source, hook);
    hook(plan, operator)
}

fn build_node<'a>(
    plan: &PhysicalPlan,
    source: &'a dyn TableSource,
    hook: &mut BuildHook<'_, 'a>,
) -> BoxedOperator<'a> {
    let mut child = |input: &PhysicalPlan| build_operator_with(input, source, hook);
    match plan {
        PhysicalPlan::SeqScan {
            table,
            projection,
            schema,
        } => Box::new(ScanExec::new(
            source,
            table.clone(),
            ScanKind::Full,
            projection.clone(),
            schema.clone(),
        )),
        PhysicalPlan::IndexScan {
            table,
            key_column,
            range,
            projection,
            schema,
        } => Box::new(ScanExec::new(
            source,
            table.clone(),
            ScanKind::Index {
                key_column: *key_column,
                range: range.clone(),
            },
            projection.clone(),
            schema.clone(),
        )),
        PhysicalPlan::IndexLookup {
            table,
            index,
            column,
            ranges,
            projection,
            schema,
            ..
        } => Box::new(ScanExec::new(
            source,
            table.clone(),
            ScanKind::Lookup {
                index: index.clone(),
                column: *column,
                ranges: ranges.clone(),
            },
            projection.clone(),
            schema.clone(),
        )),
        PhysicalPlan::Values { rows, schema } => {
            Box::new(ValuesExec::new(rows.clone(), schema.clone()))
        }
        PhysicalPlan::Filter { input, predicate } => {
            Box::new(FilterExec::new(child(input), predicate.clone()))
        }
        PhysicalPlan::Project {
            input,
            exprs,
            schema,
        } => Box::new(ProjectExec::new(
            child(input),
            exprs.clone(),
            schema.clone(),
        )),
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            join_type,
            predicate,
            schema,
        } => Box::new(NestedLoopJoinExec::new(
            child(left),
            child(right),
            *join_type,
            predicate.clone(),
            schema.clone(),
        )),
        PhysicalPlan::HashJoin {
            left,
            right,
            join_type,
            left_keys,
            right_keys,
            residual,
            schema,
        } => Box::new(HashJoinExec::new(
            child(left),
            child(right),
            *join_type,
            left_keys.clone(),
            right_keys.clone(),
            residual.clone(),
            schema.clone(),
        )),
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => Box::new(HashAggregateExec::new(
            child(input),
            group_by.clone(),
            aggregates.clone(),
            schema.clone(),
        )),
        PhysicalPlan::Sort { input, keys } => Box::new(SortExec::new(child(input), keys.clone())),
        PhysicalPlan::TopN {
            input,
            keys,
            limit,
            offset,
        } => Box::new(TopNExec::new(child(input), keys.clone(), *limit, *offset)),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => Box::new(LimitExec::new(child(input), *limit, *offset)),
        PhysicalPlan::Distinct { input } => Box::new(DistinctExec::new(child(input))),
        PhysicalPlan::SetOperation {
            op,
            all,
            left,
            right,
            schema,
        } => {
            let left = child(left);
            let right = child(right);
            Box::new(SetOpExec::new(*op, *all, left, right, schema.clone()))
        }
    }
}
