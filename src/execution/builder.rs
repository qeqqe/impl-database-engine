#![allow(dead_code, unused_variables, unused_mut)]

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
        } => todo!(),
        PhysicalPlan::IndexScan {
            table,
            key_column,
            range,
            projection,
            schema,
        } => todo!(),
        PhysicalPlan::IndexLookup {
            table,
            index,
            column,
            column_type,
            ranges,
            projection,
            schema,
        } => todo!(),
        PhysicalPlan::Values { rows, schema } => todo!(),
        PhysicalPlan::Filter { input, predicate } => todo!(),
        PhysicalPlan::Project {
            input,
            exprs,
            schema,
        } => todo!(),
        PhysicalPlan::NestedLoopJoin {
            left,
            right,
            join_type,
            predicate,
            schema,
        } => todo!(),
        PhysicalPlan::HashJoin {
            left,
            right,
            join_type,
            left_keys,
            right_keys,
            residual,
            schema,
        } => todo!(),
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
            schema,
        } => todo!(),
        PhysicalPlan::Sort { input, keys } => todo!(),
        PhysicalPlan::TopN {
            input,
            keys,
            limit,
            offset,
        } => todo!(),
        PhysicalPlan::Limit {
            input,
            limit,
            offset,
        } => todo!(),
        PhysicalPlan::Distinct { input } => todo!(),
        PhysicalPlan::SetOperation {
            op,
            all,
            left,
            right,
            schema,
        } => todo!(),
    }
}
