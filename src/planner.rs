#[allow(dead_code)]
pub mod binder;
pub mod catalog;
pub mod error;
pub mod expr;
pub mod logical;
pub mod optimizer;
pub mod physical;
pub mod physical_expr;
pub mod schema;
pub mod test_catalog;
pub mod types;
pub mod value;

use crate::{
    planner::{
        binder::Binder, catalog::Catalog, error::PlanResult, logical::Plan, optimizer::Optimizer,
    },
    sql::{Parser, Statement},
};

pub fn plan_statement(catalog: &Catalog, statement: &Statement) -> PlanResult<Plan> {
    let plan = Binder::new(catalog).bind(statement)?;
    Optimizer::default().optimize_plan(plan)
}

pub fn plan_sql(catalog: &Catalog, sql: &str) -> PlanResult<Plan> {
    plan_statement(catalog, &Parser::parse(sql)?)
}
