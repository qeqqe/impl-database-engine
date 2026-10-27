use crate::{
    execution::{
        builder::build_operator, error::ExecResult, operator::Operator, source::TableSource,
    },
    planner::{physical::PhysicalPlan, value::Row},
};

pub mod analyze;
pub mod builder;
pub mod error;
pub mod operator;
pub mod operators;
pub mod source;

pub fn collect(operator: &mut dyn Operator) -> ExecResult<Vec<Row>> {
    operator.open()?;
    let mut rows = Vec::new();
    let drained = loop {
        match operator.next() {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    let closed = operator.close();
    drained.and(closed)?;
    Ok(rows)
}

pub fn execute_query(plan: &PhysicalPlan, source: &dyn TableSource) -> ExecResult<Vec<Row>> {
    let mut root = build_operator(plan, source);
    collect(root.as_mut())
}
